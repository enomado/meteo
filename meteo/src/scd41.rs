use embassy_time::{Delay, Duration, Timer};
use esp_hal::{
    i2c::master::{Config, I2c},
    peripherals::Peripherals,
    time::Rate,
};
use esp_println::println;
use libscd::asynchronous::scd4x::Scd4x;

use crate::{
    ntp_client::{CLOCK_IS_SYNCED_WATCH, get_current_time_epoch},
    sensor::{SensorData, write_sensor_data},
};

#[embassy_executor::task]
pub async fn scd41_loop() {
    let peripherals = unsafe { Peripherals::steal() };

    let i2c = I2c::new(
        peripherals.I2C0,
        Config::default().with_frequency(Rate::from_khz(100)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO1)
    .with_scl(peripherals.GPIO2)
    .into_async();

    let mut scd = Scd4x::new(i2c, Delay);

    // остановить на случай если датчик уже измерял (после перезагрузки MCU)
    let _ = scd.stop_periodic_measurement().await;
    Timer::after(Duration::from_millis(500)).await;

    let serial = scd.serial_number().await;
    match serial {
        Ok(s) => println!("SCD41 serial: {:?}", s),
        Err(e) => {
            println!("SCD41: failed to read serial: {:?}, sensor not connected?", e);
            return;
        }
    }

    if let Err(e) = scd.start_low_power_periodic_measurement().await {
        println!("SCD41: failed to start low-power periodic measurement: {:?}", e);
        return;
    }
    println!("SCD41: low-power periodic measurement started (~30s)");

    let mut ntp_ready_receiver = CLOCK_IS_SYNCED_WATCH.receiver().unwrap();
    ntp_ready_receiver.changed_and(|s| *s == true).await;

    loop {
        Timer::after(Duration::from_secs(30)).await;

        match scd.data_ready().await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                println!("SCD41: data_ready error");
                continue;
            }
        }

        match scd.read_measurement().await {
            Ok(m) => {
                let time = get_current_time_epoch();

                println!(
                    "SCD41: CO2={} ppm, T={:.2} C, H={:.2} %",
                    m.co2, m.temperature, m.humidity
                );

                let mdata = SensorData {
                    pressure: None,
                    temp: Some(m.temperature),
                    co2: Some(m.co2),
                    humidity: Some(m.humidity),
                    time,
                };

                write_sensor_data(mdata).await;
            }
            Err(_) => {
                println!("SCD41: read error");
            }
        }
    }
}
