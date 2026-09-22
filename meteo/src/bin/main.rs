#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{
    Duration,
    Timer,
};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{
    ControllerConfig,
    PowerSaveMode,
};
use esp_rtos as _;

esp_bootloader_esp_idf::esp_app_desc!();

use meteo::led::{
    SYS_PANIC_RECOVERED,
    SYS_WDT_RECOVERED,
    init_rgb_led,
    led_loop,
    set_status,
};
use meteo::mk_static;
use meteo::network::{
    connection,
    net_task,
    network_send_loop,
    station_config,
};
use meteo::ntp_client::ntp_sync_loop;
use meteo::sensor::{
    SensorPeripherals,
    sensor_loop,
};
use meteo::watchdog::{
    BootFault,
    take_boot_fault,
    watchdog_loop,
};

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::default());

    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    // Причина этого boot'а из RTC-маркера: если предыдущий запуск упал в панику
    // или завис (watchdog), latch'им LED-бит — чтобы факт аварии был виден
    // визуально даже без serial (иначе авто-reset тихо прячет проблему).
    let boot = take_boot_fault();
    match boot.fault {
        BootFault::Panic => {
            println!(
                "BOOT: recovered from PANIC (faults this power-session: {})",
                boot.faults_since_power_on
            );
            set_status(SYS_PANIC_RECOVERED);
        }
        BootFault::WdtStall => {
            println!(
                "BOOT: recovered from WATCHDOG STALL (faults this power-session: {})",
                boot.faults_since_power_on
            );
            set_status(SYS_WDT_RECOVERED);
        }
        BootFault::Clean => println!("BOOT: clean start"),
    }

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    // esp-hal upstream #6221: софт-прерывание планировщика esp-hal резервирует
    // под RTOS сам (FROM_CPU_INTR0 из Peripherals убран), start берёт только таймер.
    esp_rtos::start(timg0.timer0);

    // RWDT-watchdog: ловит настоящие зависания (await, который не резолвится),
    // паники ловит custom_halt. Спавним после esp_rtos::start (как остальные
    // таски); feeder держит boot-grace сам (не ресетит пока таски не оживут).
    let rtc = Rtc::new(peripherals.RTC_TIMER);
    spawner.spawn(watchdog_loop(rtc.rwdt).unwrap());

    let rng = Rng::new();

    // Стартовая конфигурация — основная сеть. Если сетей в конфиге несколько,
    // `connection` перед каждым коннектом сам переставит её на самую сильную по RSSI.
    let (ssid, passwd) = WIFI_NETWORKS[0];

    let mut controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config(ssid, passwd)),
    )
    .unwrap();

    // DTIM modem-sleep: радио спит между beacon'ами, оставаясь в сети. Главная
    // экономия энергии для всегда-онлайн станции (blob-дефолт = PS_NONE, полный RX).
    // Minimum (PS_MIN_MODEM) будит на каждый DTIM — без потери downlink и почти без
    // роста латентности; шлём раз в ~30с и почти ничего не принимаем.
    controller.set_power_saving(PowerSaveMode::Minimum).unwrap();

    let wifi_interface = esp_radio::wifi::Interface::station();

    let net_config = embassy_net::Config::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<7>, StackResources::<7>::new()),
        seed,
    );

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(connection(controller).unwrap());

    // --- RGB LED (LEDC PWM) на GPIO3=R, GPIO4=G, GPIO5=B ---
    let rgb_led = init_rgb_led(
        peripherals.LEDC,
        peripherals.GPIO3,
        peripherals.GPIO4,
        peripherals.GPIO5,
    );
    spawner.spawn(led_loop(rgb_led).unwrap());

    spawner.spawn(
        sensor_loop(SensorPeripherals {
            spi2:     peripherals.SPI2,
            spi_clk:  peripherals.GPIO7,
            spi_mosi: peripherals.GPIO6,
            spi_miso: peripherals.GPIO9,
            spi_cs:   peripherals.GPIO10,
            i2c0:     peripherals.I2C0,
            i2c_sda:  peripherals.GPIO1,
            i2c_scl:  peripherals.GPIO2,
        })
        .unwrap(),
    );

    spawner.spawn(ntp_sync_loop(stack).unwrap());

    spawner.spawn(network_send_loop(stack).unwrap());

    loop {
        Timer::after(Duration::from_millis(5000)).await;
    }
}
