// ESP32-C3 использует один Embassy executor; futures датчиков не мигрируют
// между потоками, поэтому аппаратные драйверы могут оставаться !Send.
#![allow(
    clippy::future_not_send,
    reason = "аппаратные futures датчиков исполняются одним ESP32-C3 Embassy executor"
)]

//! Sensor-таска: драйвер автомата `meteo_core::sensors` на настоящих шинах.
//!
//! Что опрашивать и когда, как восстанавливаться и что считать сбоем, решает
//! автомат (с DST на хосте). Здесь — исполнение операций: каждая идёт через
//! [`perform`] под `with_timeout(op.timeout())`, поэтому ни одно ожидание шины
//! не длится вечно. Снятая по таймауту future esp-hal в своём `Drop`
//! сбрасывает автомат I2C с очисткой шины.

use bmp390_rs::error::Bmp390Error;
use bmp390_rs::register::int_status::IntStatus;
use bmp390_rs::register::pwr_ctrl::PowerMode;
use bmp390_rs::{
    Bmp390,
    ResetPolicy,
};
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_sync::blocking_mutex::raw::{
    CriticalSectionRawMutex,
    NoopRawMutex,
};
use embassy_sync::mutex::Mutex;
use embassy_time::{
    Delay,
    Duration,
    Instant,
    Timer,
    with_timeout,
};
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
use esp_hal::peripherals::{
    GPIO1,
    GPIO2,
    GPIO6,
    GPIO7,
    GPIO9,
    GPIO10,
    I2C0,
    SPI2,
};
use esp_hal::spi::master::Spi;
use esp_hal::time::Rate;
use esp_println::println;
use libscd::asynchronous::scd4x::Scd4x;
use libscd::error::Error as ScdError;
use meteo_core::backlog::{
    SensorQueue,
    push_evicting,
};
use meteo_core::codec::{
    MilliCelsius,
    Reading,
};
use meteo_core::health::{
    Health,
    Word,
};
use meteo_core::sensors::{
    Action,
    BaroRaw,
    Cycle,
    Fault,
    Op,
    OpCode,
    OpResult,
    ScdRaw,
    Sensors,
};
use meteo_core::supervisor::Uptime;
use portable_atomic::{
    AtomicU32,
    Ordering,
};

use crate::led::{
    SYS_NO_PERIPH,
    clear_co2,
    clear_status,
    publish_co2,
    set_status,
};
use crate::ntp_client::{
    CLOCK_IS_SYNCED_WATCH,
    now_epoch,
};
use crate::spi_helper::BarometerArgs;
use crate::watchdog::{
    health,
    uptime,
};

type BaroSpi = Mutex<NoopRawMutex, Spi<'static, esp_hal::Async>>;

pub type BarometerDevice<'a> =
    Bmp390<bmp390_rs::bus::Spi<SpiDevice<'a, NoopRawMutex, Spi<'static, esp_hal::Async>, Output<'a>>>>;

pub type ScdDevice<'a> = Scd4x<I2c<'a, esp_hal::Async>, Delay>;

/// Передаточная очередь показаний к сетевой таске (ёмкость и вытеснение —
/// `meteo_core::backlog`).
pub static SENSOR_QUE: Mutex<CriticalSectionRawMutex, SensorQueue> = Mutex::new(SensorQueue::new());

/// Сколько показаний вытеснено из переполненной очереди с boot. LED-бит
/// `SYS_BUF_OVERFLOW` ставит сетевая таска (один писатель) по этому счётчику
/// и по вытеснениям из своего бэклога.
pub static QUEUE_EVICTIONS: AtomicU32 = AtomicU32::new(0);

async fn enqueue_reading(reading: Reading) {
    let mut queue = SENSOR_QUE.lock().await;
    if let Some(evicted) = push_evicting(&mut queue, reading) {
        println!("queue full: dropped reading at {}", evicted.time.0);
        QUEUE_EVICTIONS.add(1, Ordering::Relaxed);
    }
}

pub struct SensorPeripherals<'a> {
    // SPI (BMP390)
    pub spi2:     SPI2<'a>,
    pub spi_clk:  GPIO7<'a>,
    pub spi_mosi: GPIO6<'a>,
    pub spi_miso: GPIO9<'a>,
    pub spi_cs:   GPIO10<'a>,
    // I2C (SCD41)
    pub i2c0:     I2C0<'a>,
    pub i2c_sda:  GPIO1<'a>,
    pub i2c_scl:  GPIO2<'a>,
}

/// Сбой SCD41 по ошибке драйвера.
const fn scd_fault(error: &ScdError<I2cError>) -> Fault {
    match error {
        ScdError::I2C(I2cError::Timeout) => Fault::Timeout,
        ScdError::I2C(_) => Fault::Bus,
        ScdError::CRC => Fault::Crc,
        ScdError::NotAllowed | ScdError::InvalidInput => Fault::Rejected,
    }
}

/// Сбой BMP390 по ошибке драйвера.
const fn baro_fault<E>(error: &Bmp390Error<E>) -> Fault {
    match error {
        Bmp390Error::Bus(_) | Bmp390Error::NotConnected => Fault::Bus,
        Bmp390Error::UnexpectedRegisterData(_) | Bmp390Error::UnexpectedFifoData => Fault::Crc,
        Bmp390Error::Timeout => Fault::Timeout,
    }
}

fn scd_done(result: Result<(), ScdError<I2cError>>) -> OpResult {
    result.map_or_else(|e| OpResult::Failed(scd_fault(&e)), |()| OpResult::Done)
}

/// Исполнить операцию автомата (кроме `BaroInit` — она открывает новую сессию
/// драйвера BMP390, см. [`sensor_loop`]). Звать только под `op.timeout()`.
async fn perform(op: Op, scd: &mut ScdDevice<'_>, baro: Option<&mut BarometerDevice<'_>>) -> OpResult {
    match op {
        Op::ScdStop => scd_done(scd.stop_periodic_measurement().await),
        Op::ScdReinit => scd_done(scd.reinit().await),
        Op::ScdSerial => scd_done(scd.serial_number().await.map(|_| ())),
        Op::ScdGetTempOffset => {
            match scd.get_temperature_offset().await {
                Ok(c) => {
                    MilliCelsius::from_celsius(c)
                        .map_or(OpResult::Failed(Fault::BadValue), OpResult::TempOffset)
                }
                Err(e) => OpResult::Failed(scd_fault(&e)),
            }
        }
        Op::ScdSetTempOffset(offset) => {
            // Смещение — единицы °C, в f32 точно.
            #[allow(clippy::cast_possible_truncation)]
            let celsius = offset.celsius() as f32;
            scd_done(scd.set_temperature_offset(celsius).await)
        }
        Op::ScdStart => scd_done(scd.start_low_power_periodic_measurement().await),
        Op::ScdDataReady => {
            scd.data_ready()
                .await
                .map_or_else(|e| OpResult::Failed(scd_fault(&e)), OpResult::Ready)
        }
        Op::ScdRead => {
            scd.read_measurement().await.map_or_else(
                |e| OpResult::Failed(scd_fault(&e)),
                |m| {
                    OpResult::Scd(ScdRaw {
                        co2:      m.co2,
                        temp_c:   m.temperature,
                        humidity: m.humidity,
                    })
                },
            )
        }
        Op::ScdSetPressure(hpa) => scd_done(scd.set_ambient_pressure(hpa.0).await),
        Op::BaroPoll => {
            // Автомат опрашивает BMP390 только после удачной инициализации
            // этой сессии (DST: «polled before init»).
            poll_barometer(baro.expect("BMP390 is polled only after its init succeeded")).await
        }
        Op::BaroInit => unreachable!("BaroInit opens a new barometer session in sensor_loop"),
    }
}

/// Один опрос BMP390: статус `drdy` → чтение.
async fn poll_barometer(barometer: &mut BarometerDevice<'_>) -> OpResult {
    match barometer.read::<IntStatus>().await {
        Ok(status) if !status.drdy => OpResult::Baro(None),
        Ok(_) => {
            barometer.read_sensor_data().await.map_or_else(
                |e| OpResult::Failed(baro_fault(&e)),
                |data| {
                    OpResult::Baro(Some(BaroRaw {
                        pressure_pa: data.pressure(),
                        temp_c:      data.temperature(),
                    }))
                },
            )
        }
        Err(e) => OpResult::Failed(baro_fault(&e)),
    }
}

/// Создать драйвер BMP390 на CS `cs` (проверка id, запись конфигурации).
async fn init_barometer<'a>(spi_bus: &'a BaroSpi, cs: GPIO10<'a>) -> Result<BarometerDevice<'a>, Fault> {
    let cs = Output::new(cs, Level::High, OutputConfig::default());
    let spi_device = SpiDevice::new(spi_bus, cs);

    // Конфигурация пишется в Sleep, normal-режим включается последним: ресет
    // MCU барометр не обесточивает, он остаётся в normal прошлой жизни, а
    // запись Normal поверх Normal цикл не перезапускает — первый `drdy` пришёл
    // бы по старой фазе ODR, через 20 с (плата). Sleep → Normal начинает
    // свежий цикл с преобразования (`sensors::BARO_CONVERSION`).
    let config = bmp390_rs::config::Configuration::default()
        .power_mode(PowerMode::Sleep)
        .output_data_rate(bmp390_rs::register::odr::OutputDataRate::R0p05Hz) // ~20 сек
        .pressure_oversampling(bmp390_rs::register::osr::Oversampling::X32)
        .temperature_oversampling(bmp390_rs::register::osr::Oversampling::X8)
        .iir_filter_coefficient(bmp390_rs::register::config::IIRFilterCoefficient::Coef3);

    // ResetPolicy::Soft (CMD=0xB6 + ожидание `cmd_rdy`) не успевает
    // сконфигурироваться и молчит — циферки не меняет. Поэтому без сброса,
    // с паузой на подачу питания.
    Timer::after(Duration::from_millis(150)).await;
    let init = async {
        let mut device = Bmp390::new_spi(spi_device, config, ResetPolicy::None, &mut Delay).await?;
        device.set_mode(PowerMode::Normal).await?;
        Ok(device)
    };
    init.await.map_err(|e| {
        println!("BMP390: init failed: {:?}", e);
        baro_fault(&e)
    })
}

fn instant(at: Uptime) -> Instant {
    Instant::from_micros(u64::try_from(at.0.as_micros()).unwrap_or(u64::MAX))
}

fn report_failure(op: Op, result: &OpResult) {
    if let OpResult::Failed(fault) = result {
        println!("{}: {:?}", op.code().name(), fault);
    }
}

/// Итог цикла: LED, `SYS_NO_PERIPH`, показание в очередь (после первого NTP).
async fn finish_cycle(cycle: Cycle, ntp_synced: bool) {
    let s = cycle.sample;
    match s.co2 {
        Some(co2) => publish_co2(co2),
        None => clear_co2(),
    }
    if cycle.periph_ok {
        clear_status(SYS_NO_PERIPH);
    } else {
        set_status(SYS_NO_PERIPH);
    }
    println!(
        "cycle: co2={:?} T_scd={:?} H={:?} P={:?} T_bmp={:?}",
        s.co2.map(|c| c.0),
        s.scd_temp.map(|t| t.0),
        s.humidity.map(|h| h.0),
        s.pressure.map(|p| p.0),
        s.baro_temp.map(|t| t.0),
    );
    // В серверный буфер — только с доверенным временем: до первого NTP-синка
    // часы идут от вшитого значения. Показание без каналов не кладём.
    if ntp_synced {
        let reading = s.into_reading(now_epoch());
        if reading.has_any_channel() {
            enqueue_reading(reading).await;
        }
    }
}

/// Исполнить операцию под её пределом, с этапом таски в журнале.
async fn run_op(
    op: Op,
    scd: &mut ScdDevice<'_>,
    baro: Option<&mut BarometerDevice<'_>>,
    health: &Health<'_>,
) -> OpResult {
    health.set(Word::SensorStage, op.code().0);
    let result = with_timeout(Duration::from_micros(op_timeout_us(op)), perform(op, scd, baro))
        .await
        .unwrap_or(OpResult::Failed(Fault::Timeout));
    health.set(Word::SensorStage, OpCode::WAIT.0);
    report_failure(op, &result);
    result
}

fn op_timeout_us(op: Op) -> u64 {
    u64::try_from(op.timeout().as_micros()).expect("operation timeouts are seconds")
}

#[embassy_executor::task]
pub async fn sensor_loop(p: SensorPeripherals<'static>) {
    let SensorPeripherals {
        spi2,
        spi_clk,
        spi_mosi,
        spi_miso,
        mut spi_cs,
        i2c0,
        i2c_sda,
        i2c_scl,
    } = p;

    let spi_bus = crate::spi_helper::init_spi_bus(BarometerArgs {
        spi2,
        clk: spi_clk,
        mosi: spi_mosi,
        miso: spi_miso,
    });

    let i2c = I2c::new(i2c0, I2cConfig::default().with_frequency(Rate::from_khz(100)))
        .expect("the default I2C config at 100 kHz is valid")
        .with_sda(i2c_sda)
        .with_scl(i2c_scl)
        .into_async();
    let mut scd = Scd4x::new(i2c, Delay);

    let health = health();
    let mut sensors = Sensors::new(uptime());
    let mut ntp_ready = CLOCK_IS_SYNCED_WATCH
        .receiver()
        .expect("CLOCK_IS_SYNCED_WATCH has a receiver slot for the sensor task");

    // Сессии драйвера BMP390. Драйвер забирает SPI-устройство с CS по значению
    // и при неудачной инициализации его не возвращает ⇒ инициализация берёт
    // свежий `reborrow` CS, а её срок — одна итерация внешнего цикла: `BaroInit`
    // закрывает сессию, следующая итерация создаёт драйвер заново.
    let mut init_requested = false;
    loop {
        let mut baro: Option<BarometerDevice<'_>> = None;
        if init_requested {
            health.set(Word::SensorStage, Op::BaroInit.code().0);
            let result = match with_timeout(
                Duration::from_micros(op_timeout_us(Op::BaroInit)),
                init_barometer(spi_bus, spi_cs.reborrow()),
            )
            .await
            {
                Ok(Ok(device)) => {
                    println!("BMP390: init ok");
                    baro = Some(device);
                    OpResult::Done
                }
                Ok(Err(fault)) => OpResult::Failed(fault),
                Err(_) => OpResult::Failed(Fault::Timeout),
            };
            health.set(Word::SensorStage, OpCode::WAIT.0);
            report_failure(Op::BaroInit, &result);
            sensors.done(result, uptime(), &health);
        }

        // Единственный выход из цикла автомата — запрос `BaroInit`.
        loop {
            match sensors.next(uptime(), &health) {
                Action::Run(Op::BaroInit) => break,
                Action::Run(op) => {
                    let result = run_op(op, &mut scd, baro.as_mut(), &health).await;
                    sensors.done(result, uptime(), &health);
                }
                Action::Wait(until) => Timer::at(instant(until)).await,
                Action::Cycle(cycle) => {
                    // heartbeat для watchdog: цикл автомата ограничен по времени
                    // при любом поведении шины (DST, инвариант 1).
                    crate::watchdog::beat_sensor();
                    finish_cycle(cycle, ntp_ready.try_get() == Some(true)).await;
                }
            }
        }
        init_requested = true;
    }
}
