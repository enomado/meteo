use bmp390_rs::{Bmp390, ResetPolicy, register::int_status::IntStatus};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    mutex::Mutex,
};
use embassy_time::{Delay, Duration, Timer};
use esp_hal::{
    gpio::{Level, Output, OutputConfig},
    peripherals::{GPIO8, Peripherals},
    spi::master::Spi,
};
use esp_println::println;

use crate::{
    ntp_client::{CLOCK_IS_SYNCED_WATCH, get_current_time_epoch},
    spi_helper::init_spi_bus_new2,
};

pub type BarometerDevice<'a> = Bmp390<
    bmp390_rs::bus::Spi<
        embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice<
            'a,
            NoopRawMutex,
            Spi<'a, esp_hal::Async>,
            Output<'a>,
        >,
    >,
>;

pub async fn get_barometer_spi<'a>(
    spi_bus: &'a Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>>,
    cs_pin: GPIO8<'a>,
) -> BarometerDevice<'a> {
    // Measurement: 87262.42 23.572037
    // https://github.com/EmilNorden/bmp390-rs

    let cs_pin = Output::new(cs_pin, Level::High, OutputConfig::default());

    // let spi_bus: Mutex<NoopRawMutex, Spi<'_, esp_hal::Async>> =
    // Mutex::<NoopRawMutex, _>::new(spi_bus);
    let spi_device = embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice::new(spi_bus, cs_pin);

    let mut delay = Delay;

    let config = bmp390_rs::config::Configuration::default()
        .output_data_rate(bmp390_rs::register::odr::OutputDataRate::R0p1Hz) // работает
        .pressure_oversampling(bmp390_rs::register::osr::Oversampling::X32)
        .temperature_oversampling(bmp390_rs::register::osr::Oversampling::X8)
        .iir_filter_coefficient(bmp390_rs::register::config::IIRFilterCoefficient::Coef3);

    // если ResetPolicy::Soft
    // Issue CMD=0xB6 and wait for `STATUS.cmd_rdy` (recommended default).
    // о не успевает законфигурироваться и молчит - циферки не меняет

    // Connect via SPI. Use Bmp390::new_i2c for I2C.
    let device: BarometerDevice<'a> =
        Bmp390::new_spi(spi_device, config, ResetPolicy::None, &mut delay)
            .await
            .unwrap();

    device
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SensorData {
    pub pressure: f32,
    pub temp: f32,
    // millis epoch
    pub time: u64,
}

// pub type ChannelType = Channel<CriticalSectionRawMutex, SensorData, 3>;
// pub static CHANNEL: ChannelType = Channel::new();

use heapless::spsc::{Producer, Queue};
// pub static SENSOR_QUE: Queue<SensorData, 4> = Queue::new();

// use heapless::HistoryBuffer;

// pub static SENSOR_QUE: Mutex<CriticalSectionRawMutex, HistoryBuffer<SensorData, 200>> =
//     Mutex::new(HistoryBuffer::new());

// NoopRawMutex
pub static SENSOR_QUE: Mutex<CriticalSectionRawMutex, Queue<SensorData, 60>> =
    Mutex::new(Queue::new());

#[embassy_executor::task]
pub async fn sensor_loop_new() {
    let peripherals = unsafe { Peripherals::steal() };

    let spi_bus = init_spi_bus_new2().await;
    let mut barometer = get_barometer_spi(&spi_bus, peripherals.GPIO8).await;

    let p = barometer.max_measurement_time_us();

    let _data = barometer.read_sensor_data().await.unwrap();

    let mut ntp_ready_receiver = CLOCK_IS_SYNCED_WATCH.receiver().unwrap();

    ntp_ready_receiver.changed_and(|s| *s == true).await;

    println!("time is: {}", p);

    loop {
        let status2 = barometer.read::<IntStatus>().await.unwrap();

        if status2.drdy {
            let data = barometer.read_sensor_data().await.unwrap();

            let time = get_current_time_epoch();

            let mdata = SensorData {
                pressure: data.pressure(),
                temp: data.temperature(),
                time: time,
            };

            // CHANNEL.send(mdata).await;

            println!(
                "Measurement: {:?} {:?} {:?}",
                data.pressure(),
                data.temperature(),
                status2.drdy
            );

            write_sensor_data(mdata).await;
        }

        Timer::after(Duration::from_micros(p.into())).await;
    }
}

pub async fn write_sensor_data(mdata: SensorData) {
    // return;

    let mut p = SENSOR_QUE.lock().await;
    // sort of ring buffer

    match p.enqueue(mdata) {
        Ok(x) => {}
        Err(el) => {
            p.dequeue().unwrap();
            p.enqueue(el).unwrap();
        }
    }
}
