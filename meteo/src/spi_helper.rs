use bmp390_rs::{Bmp390, ResetPolicy, register::int_status::IntStatus};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    channel::Channel,
    mutex::Mutex,
};
use embassy_time::{Delay, Duration, Timer};
use esp_hal::{
    gpio::{Level, Output, OutputConfig},
    peripherals::{GPIO5, GPIO6, GPIO7, GPIO8, Peripherals, SPI2},
    spi::{self, master::Spi},
};
use esp_println::println;

pub struct BarometerArgs<'a> {
    // .with_sck(peripherals.GPIO7)
    pub clk: GPIO7<'a>,
    // .with_mosi(peripherals.GPIO6)
    pub mosi: GPIO6<'a>,
    // .with_miso(peripherals.GPIO5);
    pub miso: GPIO5<'a>,

    pub spi2: SPI2<'a>,
}

pub async fn init_spi_bus_new2<'a>() -> Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>> {
    let peripherals = unsafe { Peripherals::steal() };

    let args: BarometerArgs<'a> = BarometerArgs {
        spi2: peripherals.SPI2,
        clk: peripherals.GPIO7,
        mosi: peripherals.GPIO6,
        miso: peripherals.GPIO5,
    };

    let spi_bus = init_spi_bus_new(args);
    // bmp390_rs::bus::Spi::new()

    // use embedded_hal_bus::spi::ExclusiveDevice;

    // let spi_dev = ExclusiveDevice::new_no_delay(spi_bus, peripherals.GPIO8).unwrap();

    // let mut barometer = get_barometer_spi(&spi_device, peripherals.GPIO8).await;

    spi_bus
}

fn init_spi_bus_new<'a>(args: BarometerArgs<'a>) -> Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>> {
    // let spi_device: Mutex<NoopRawMutex, Spi<'_, esp_hal::Async>> = init_barometer_spi(args).await;

    let spi_bus = Spi::new(
        args.spi2,
        spi::master::Config::default(), // .with_mode(Mode::_0),
    )
    .unwrap();

    println!("spi2");

    let spi_bus = spi_bus
        .with_sck(args.clk)
        .with_mosi(args.mosi)
        .with_miso(args.miso);

    println!("spi22");

    let spi_bus = spi_bus.into_async();

    let spi_bus: Mutex<NoopRawMutex, Spi<'_, esp_hal::Async>> =
        Mutex::<NoopRawMutex, _>::new(spi_bus);

    spi_bus
}

pub async fn init_barometer_spi<'a>(
    peripherals: BarometerArgs<'a>,
) -> Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>> {
    // 20 SPICS0 SPICS0 / IO14
    // 21 SPICLK SPICLK / IO15
    // 22 SPIQ SPIQ / IO17           MOSI
    // 23 SPID SPID / IO16           MISO

    // .with_frequency(Rate::from_mhz(6))
    println!("spi1");
    // Initialize SPI
    let spi_bus = Spi::new(
        peripherals.spi2,
        spi::master::Config::default(), // .with_mode(Mode::_0),
    )
    .unwrap();

    let spi_bus = spi_bus
        .with_sck(peripherals.clk)
        .with_mosi(peripherals.mosi)
        .with_miso(peripherals.miso);

    let spi_bus = spi_bus.into_async();

    //CLK
    //DIN
    // .with_mosi(peripherals.GPIO23);

    // https://github.com/esp-rs/esp-hal/blob/main/examples/peripheral/spi/loopback/src/main.rs

    // допустим, мы хотим взять GPIO5 как CS

    // let mut cs_pin = Output::new(peripherals.GPIO14, Level::High, OutputConfig::default());
    // let cs = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());
    // let mut spi_device = ExclusiveDevice::new(spi_bus, cs_pin, Timer::after).unwrap();

    let spi_bus: Mutex<NoopRawMutex, Spi<'_, esp_hal::Async>> =
        Mutex::<NoopRawMutex, _>::new(spi_bus);

    spi_bus
}

// pub struct MySpiDev<'a> {
//     spi_bus: Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>>,
//     spi_device: SpiDevice<'a, NoopRawMutex, Spi<'a, esp_hal::Async>, Output<'a>>,
// }
