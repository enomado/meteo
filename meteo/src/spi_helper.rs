use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex,
    mutex::Mutex,
};
use esp_hal::{
    peripherals::{GPIO6, GPIO7, GPIO9, SPI2},
    spi::{self, master::Spi},
};
use esp_println::println;

pub struct BarometerArgs<'a> {
    pub clk: GPIO7<'a>,
    pub mosi: GPIO6<'a>,
    pub miso: GPIO9<'a>,
    pub spi2: SPI2<'a>,
}

pub fn init_spi_bus<'a>(args: BarometerArgs<'a>) -> Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>> {
    let spi_bus = Spi::new(
        args.spi2,
        spi::master::Config::default(),
    )
    .unwrap();

    println!("spi2");

    let spi_bus = spi_bus
        .with_sck(args.clk)
        .with_mosi(args.mosi)
        .with_miso(args.miso);

    println!("spi22");

    let spi_bus = spi_bus.into_async();

    Mutex::<NoopRawMutex, _>::new(spi_bus)
}
