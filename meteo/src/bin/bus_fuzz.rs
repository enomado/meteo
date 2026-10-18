//! Bus fuzzer: hammer the SCD41 (I2C) and BMP390 (SPI) on the real board while
//! Wi-Fi keeps the radio busy, and report every bus operation that does not
//! finish within `OP_LIMIT`, with the I2C peripheral state at that moment.
//!
//! The production firmware bounds every operation by a timeout; this binary
//! looks for the hang itself: which operation, in which bus state, how often.
//!
//! Only documented SCD41 commands that touch neither EEPROM nor calibration are
//! sent (no `persist_settings`, factory reset, forced recalibration, setters or
//! undocumented opcodes). The fuzzing is in how they are sent: in any order
//! regardless of the sensor state, without waiting for the execution time,
//! with wrong lengths and bad CRCs, reads of random length or with no command
//! before them, and transfers to an absent address.
//!
//! Run: `cargo run --release --bin bus_fuzz` (the board then stops logging
//! data until the regular firmware is flashed back). Every `REPORT_EVERY` the
//! counters go to the serial console; a seed is printed at boot.

#![no_std]
#![no_main]
#![allow(
    clippy::future_not_send,
    reason = "all tasks run on the single ESP32-C3 Embassy executor"
)]

use bmp390_rs::register::int_status::IntStatus;
use bmp390_rs::{
    Bmp390,
    ResetPolicy,
};
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_executor::Spawner;
use embassy_net::udp::{
    PacketMetadata,
    UdpSocket,
};
use embassy_net::{
    DhcpConfig,
    Stack,
    StackResources,
};
use embassy_time::{
    Delay,
    Duration,
    Instant,
    Timer,
    with_timeout,
};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{
    Level,
    Output,
    OutputConfig,
};
use esp_hal::i2c::master::{
    Config as I2cConfig,
    Error as I2cError,
    I2c,
};
use esp_hal::peripherals::I2C0;
use esp_hal::rng::Rng;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{
    ControllerConfig,
    PowerSaveMode,
};
use esp_rtos as _;
use meteo::mk_static;
use meteo::network::{
    connection,
    net_task,
    primary_network,
    station_config,
};
use meteo::spi_helper::{
    BarometerArgs,
    init_spi_bus,
};
use portable_atomic::{
    AtomicU32,
    Ordering,
};

esp_bootloader_esp_idf::esp_app_desc!();

const SCD41: u8 = 0x62;
/// Nothing answers here: every transfer must end in a NACK, not a hang.
const ABSENT: u8 = 0x2A;
/// A bus operation longer than this is a hang (the slowest legal command,
/// `stop_periodic_measurement`, is 500 ms and is not awaited here).
const OP_LIMIT: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);
/// Pause between Wi-Fi load datagrams.
const RADIO_LOAD_PERIOD: Duration = Duration::from_millis(50);

/// SCD41 commands that are safe to send in any state: none of them writes
/// EEPROM or changes calibration. `(opcode, response length)`.
const COMMANDS: [(u16, usize); 9] = [
    (0x21b1, 0), // start_periodic_measurement
    (0x21ac, 0), // start_low_power_periodic_measurement
    (0x3f86, 0), // stop_periodic_measurement
    (0xe4b8, 3), // get_data_ready_status
    (0xec05, 9), // read_measurement
    (0x3682, 9), // get_serial_number
    (0x2318, 3), // get_temperature_offset
    (0x3646, 0), // reinit
    (0x219d, 0), // measure_single_shot
];
/// `set_ambient_pressure` — RAM only, overwritten by the next valid write.
const SET_PRESSURE: u16 = 0xe000;

/// Sensirion CRC-8 (poly 0x31, init 0xFF) over one 16-bit word.
fn crc8(data: [u8; 2]) -> u8 {
    let mut crc = 0xFFu8;
    for byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x31
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// xorshift64*: seeded once from the hardware RNG so a run can be described
/// by its seed.
struct Fuzz(u64);

impl Fuzz {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    const fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(len as u64)).expect("index below a slice length fits usize")
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    const fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }
}

/// One fuzzed I2C transfer.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// A command, its response read right away (no execution wait).
    Command { opcode: u16, read: usize },
    /// A command, then the response after `wait_ms`.
    CommandThenRead {
        opcode:  u16,
        read:    usize,
        wait_ms: u64,
    },
    /// `set_ambient_pressure` with a payload of `len` bytes, CRC good or bad.
    Pressure {
        hpa:      u16,
        good_crc: bool,
        extra:    usize,
    },
    /// A read with no command before it.
    BareRead { len: usize },
    /// A transfer to an address nobody answers.
    Absent { read: bool },
}

#[derive(Default)]
struct Stats {
    ops:         u32,
    ok:          u32,
    nack:        u32,
    timeout_hw:  u32,
    arbitration: u32,
    other:       u32,
    hangs:       u32,
    max_us:      u64,
}

static BARO_POLLS: AtomicU32 = AtomicU32::new(0);
static BARO_ERRORS: AtomicU32 = AtomicU32::new(0);
static BARO_HANGS: AtomicU32 = AtomicU32::new(0);
static RADIO_SENT: AtomicU32 = AtomicU32::new(0);

fn pick_step(f: &mut Fuzz) -> Step {
    let (opcode, read) = COMMANDS[f.index(COMMANDS.len())];
    match f.below(10) {
        0..=3 => {
            Step::Command {
                opcode,
                // Sometimes the wrong response length.
                read: if f.chance(20) { f.index(12) } else { read },
            }
        }
        4..=6 => {
            Step::CommandThenRead {
                opcode,
                read: if f.chance(20) { f.index(12) } else { read },
                wait_ms: f.below(6),
            }
        }
        7 => {
            Step::Pressure {
                hpa:      700 + u16::try_from(f.below(500)).expect("below 500 fits u16"),
                good_crc: f.chance(70),
                extra:    f.index(3),
            }
        }
        8 => Step::BareRead { len: 1 + f.index(12) },
        _ => Step::Absent { read: f.chance(50) },
    }
}

async fn perform(i2c: &mut I2c<'_, esp_hal::Async>, step: Step, f: &mut Fuzz) -> Result<(), I2cError> {
    let mut rx = [0u8; 12];
    match step {
        Step::Command { opcode, read } => {
            i2c.write_async(SCD41, &opcode.to_be_bytes()).await?;
            if read > 0 {
                i2c.read_async(SCD41, &mut rx[..read]).await?;
            }
        }
        Step::CommandThenRead {
            opcode,
            read,
            wait_ms,
        } => {
            i2c.write_async(SCD41, &opcode.to_be_bytes()).await?;
            Timer::after(Duration::from_millis(wait_ms)).await;
            if read > 0 {
                i2c.read_async(SCD41, &mut rx[..read]).await?;
            }
        }
        Step::Pressure { hpa, good_crc, extra } => {
            let word = hpa.to_be_bytes();
            let crc = if good_crc { crc8(word) } else { crc8(word) ^ 0x5A };
            let [op_hi, op_lo] = SET_PRESSURE.to_be_bytes();
            let mut tx = [op_hi, op_lo, word[0], word[1], crc, 0, 0];
            for b in &mut tx[5..5 + extra] {
                *b = f.byte();
            }
            i2c.write_async(SCD41, &tx[..5 + extra]).await?;
        }
        Step::BareRead { len } => i2c.read_async(SCD41, &mut rx[..len]).await?,
        Step::Absent { read } => {
            if read {
                i2c.read_async(ABSENT, &mut rx[..3]).await?;
            } else {
                i2c.write_async(ABSENT, &[0xE4, 0xB8]).await?;
            }
        }
    }
    Ok(())
}

/// I2C peripheral state when an operation hangs.
fn dump_i2c() {
    let regs = I2C0::regs();
    println!(
        "  i2c: sr={:#010x} int_raw={:#010x} int_ena={:#010x} fifo_st={:#010x} ctr={:#010x}",
        regs.sr().read().bits(),
        regs.int_raw().read().bits(),
        regs.int_ena().read().bits(),
        regs.fifo_st().read().bits(),
        regs.ctr().read().bits(),
    );
    for (i, cmd) in regs.comd_iter().enumerate() {
        println!("  i2c: comd[{}]={:#010x}", i, cmd.read().bits());
    }
}

fn report(stats: &Stats, since: Instant) {
    println!(
        "fuzz {}s: i2c ops {} ok {} nack {} hw_timeout {} arbitration {} other {} HANGS {} max {} us | bmp390 polls {} errors {} HANGS {} | radio datagrams {}",
        since.elapsed().as_secs(),
        stats.ops,
        stats.ok,
        stats.nack,
        stats.timeout_hw,
        stats.arbitration,
        stats.other,
        stats.hangs,
        stats.max_us,
        BARO_POLLS.load(Ordering::Relaxed),
        BARO_ERRORS.load(Ordering::Relaxed),
        BARO_HANGS.load(Ordering::Relaxed),
        RADIO_SENT.load(Ordering::Relaxed),
    );
}

#[embassy_executor::task]
async fn i2c_fuzz(mut i2c: I2c<'static, esp_hal::Async>, seed: u64) {
    let mut f = Fuzz(seed);
    let mut stats = Stats::default();
    let started = Instant::now();
    let mut next_report = started + REPORT_EVERY;
    loop {
        let step = pick_step(&mut f);
        let t0 = Instant::now();
        let outcome = with_timeout(OP_LIMIT, perform(&mut i2c, step, &mut f)).await;
        let took = t0.elapsed().as_micros();
        stats.ops += 1;
        stats.max_us = stats.max_us.max(took);
        match outcome {
            Ok(Ok(())) => stats.ok += 1,
            Ok(Err(I2cError::AcknowledgeCheckFailed(_))) => stats.nack += 1,
            Ok(Err(I2cError::Timeout)) => stats.timeout_hw += 1,
            Ok(Err(I2cError::ArbitrationLost)) => stats.arbitration += 1,
            Ok(Err(_)) => stats.other += 1,
            Err(_) => {
                stats.hangs += 1;
                println!(
                    "HANG #{} after {} ops: {:?} ({} us)",
                    stats.hangs, stats.ops, step, took
                );
                dump_i2c();
            }
        }
        if Instant::now() >= next_report {
            report(&stats, started);
            next_report += REPORT_EVERY;
        }
        // Mostly back to back; sometimes a pause, so the sensor finishes what
        // it was doing and the next command lands in a different state.
        if f.chance(5) {
            Timer::after(Duration::from_millis(f.below(600))).await;
        }
    }
}

type BaroSpi = embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::NoopRawMutex,
    esp_hal::spi::master::Spi<'static, esp_hal::Async>,
>;

#[embassy_executor::task]
async fn baro_load(spi_bus: &'static BaroSpi, cs: esp_hal::peripherals::GPIO10<'static>) {
    let cs = Output::new(cs, Level::High, OutputConfig::default());
    let device = SpiDevice::new(spi_bus, cs);
    let config = bmp390_rs::config::Configuration::default();
    Timer::after(Duration::from_millis(150)).await;
    let Ok(mut baro) = Bmp390::new_spi(device, config, ResetPolicy::None, &mut Delay).await else {
        println!("bmp390: init failed, SPI load disabled");
        return;
    };
    loop {
        match with_timeout(OP_LIMIT, baro.read::<IntStatus>()).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                BARO_ERRORS.add(1, Ordering::Relaxed);
            }
            Err(_) => {
                BARO_HANGS.add(1, Ordering::Relaxed);
                println!("HANG bmp390 status read");
            }
        }
        BARO_POLLS.add(1, Ordering::Relaxed);
        Timer::after(Duration::from_millis(7)).await;
    }
}

/// Keeps the radio busy: small datagrams to the gateway's discard port.
#[embassy_executor::task]
async fn radio_load(stack: Stack<'static>) {
    stack.wait_config_up().await;
    let Some(gateway) = stack.config_v4().and_then(|c| c.gateway) else {
        println!("radio: no gateway, radio load disabled");
        return;
    };
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx = [0u8; 64];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx = [0u8; 512];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket
        .bind(0)
        .expect("a fresh UDP socket binds to an ephemeral port");
    let payload = [0x55u8; 96];
    loop {
        if socket.send_to(&payload, (gateway, 9)).await.is_ok() {
            RADIO_SENT.add(1, Ordering::Relaxed);
        }
        Timer::after(RADIO_LOAD_PERIOD).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::default()));
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    let rng = Rng::new();
    let mut controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config(primary_network())),
    )
    .expect("Wi-Fi controller starts");
    controller
        .set_power_saving(PowerSaveMode::Minimum)
        .expect("power saving mode is accepted");
    let seed_net = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let (stack, runner) = embassy_net::new(
        esp_radio::wifi::Interface::station(),
        embassy_net::Config::dhcpv4(DhcpConfig::default()),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed_net,
    );
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(connection(controller).unwrap());
    spawner.spawn(radio_load(stack).unwrap());

    let spi_bus = init_spi_bus(BarometerArgs {
        spi2: peripherals.SPI2,
        clk:  peripherals.GPIO7,
        mosi: peripherals.GPIO6,
        miso: peripherals.GPIO9,
    });
    spawner.spawn(baro_load(spi_bus, peripherals.GPIO10).unwrap());

    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(100)),
    )
    .expect("the default I2C config at 100 kHz is valid")
    .with_sda(peripherals.GPIO1)
    .with_scl(peripherals.GPIO2)
    .into_async();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random()) | 1;
    println!("bus_fuzz: seed {:#018x}", seed);
    spawner.spawn(i2c_fuzz(i2c, seed).unwrap());

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
