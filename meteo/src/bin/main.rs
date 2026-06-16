#![no_std]
#![no_main]

use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::{clock::CpuClock, rng::Rng, timer::timg::TimerGroup};

use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};

use esp_println as _;

use esp_backtrace as _;

use esp_alloc as _;

use esp_radio::wifi::{Config, ControllerConfig, sta::StationConfig};
use esp_rtos as _;

esp_bootloader_esp_idf::esp_app_desc!();

use meteo::mk_static;
use meteo::ntp_client::ntp_sync_loop;
use meteo::{
    network::{connection, net_task, network_send_loop},
    sensor::{SensorPeripherals, sensor_loop},
};

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::default());

    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    let rng = Rng::new();

    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWD.into()),
    );

    let controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .unwrap();

    let wifi_interface = esp_radio::wifi::Interface::station();

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<7>, StackResources::<7>::new()),
        seed,
    );

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(connection(controller).unwrap());

    // --- RGB LED (LEDC PWM) на GPIO3=R, GPIO4=G, GPIO5=B ---
    let rgb_led = meteo::led::init_rgb_led(
        peripherals.LEDC,
        peripherals.GPIO3,
        peripherals.GPIO4,
        peripherals.GPIO5,
    );
    spawner.spawn(meteo::led::led_loop(rgb_led).unwrap());

    spawner.spawn(
        sensor_loop(SensorPeripherals {
            spi2: peripherals.SPI2,
            spi_clk: peripherals.GPIO7,
            spi_mosi: peripherals.GPIO6,
            spi_miso: peripherals.GPIO9,
            spi_cs: peripherals.GPIO10,
            i2c0: peripherals.I2C0,
            i2c_sda: peripherals.GPIO1,
            i2c_scl: peripherals.GPIO2,
        })
        .unwrap(),
    );

    spawner.spawn(ntp_sync_loop(stack).unwrap());

    spawner.spawn(network_send_loop(stack).unwrap());

    loop {
        Timer::after(Duration::from_millis(5000)).await;
    }
}
