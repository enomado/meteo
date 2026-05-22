use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use esp_hal::{
    peripherals::{GPIO6, GPIO7, GPIO9, SPI2},
    spi::{
        Mode,
        master::{Config, Spi},
    },
    time::Rate,
};

pub struct BarometerArgs<'a> {
    pub clk: GPIO7<'a>,
    pub mosi: GPIO6<'a>,
    pub miso: GPIO9<'a>,
    pub spi2: SPI2<'a>,
}

pub fn init_spi_bus(
    args: BarometerArgs<'static>,
) -> &'static Mutex<NoopRawMutex, Spi<'static, esp_hal::Async>> {
    let spi_bus = Spi::new(
        args.spi2,
        Config::default()
            .with_frequency(Rate::from_khz(100))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(args.clk)
    .with_mosi(args.mosi)
    .with_miso(args.miso)
    .into_async();

    crate::mk_static!(Mutex<NoopRawMutex, Spi<'static, esp_hal::Async>>, Mutex::new(spi_bus))
}
