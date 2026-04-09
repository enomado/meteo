use bmp390_rs::{Bmp390, ResetPolicy, register::int_status::IntStatus};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    mutex::Mutex,
};
use embassy_time::{Delay, Duration, Timer};
use esp_hal::{
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    peripherals::{GPIO1, GPIO2, GPIO6, GPIO7, GPIO9, GPIO10, I2C0, SPI2},
    spi::master::Spi,
    time::Rate,
};
use esp_println::println;
use heapless::spsc::Queue;
use libscd::asynchronous::scd4x::Scd4x;

use crate::{
    ntp_client::{CLOCK_IS_SYNCED_WATCH, get_current_time_epoch},
    spi_helper::BarometerArgs,
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
    cs_pin: GPIO10<'a>,
) -> BarometerDevice<'a> {
    let cs_pin = Output::new(cs_pin, Level::High, OutputConfig::default());
    let spi_device = embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice::new(spi_bus, cs_pin);

    let mut delay = Delay;

    let config = bmp390_rs::config::Configuration::default()
        .output_data_rate(bmp390_rs::register::odr::OutputDataRate::R0p05Hz) // ~20 сек
        .pressure_oversampling(bmp390_rs::register::osr::Oversampling::X32)
        .temperature_oversampling(bmp390_rs::register::osr::Oversampling::X8)
        .iir_filter_coefficient(bmp390_rs::register::config::IIRFilterCoefficient::Coef3);

    // если ResetPolicy::Soft
    // Issue CMD=0xB6 and wait for `STATUS.cmd_rdy` (recommended default).
    // но не успевает законфигурироваться и молчит - циферки не меняет

    let device: BarometerDevice<'a> =
        Bmp390::new_spi(spi_device, config, ResetPolicy::None, &mut delay)
            .await
            .unwrap();

    device
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SensorData {
    pub pressure: Option<f32>,
    pub temp: Option<f32>,
    pub co2: Option<u16>,
    pub humidity: Option<f32>,
    pub scd_temp: Option<f32>,
    // millis epoch
    pub time: u64,
}

pub static SENSOR_QUE: Mutex<CriticalSectionRawMutex, Queue<SensorData, 60>> =
    Mutex::new(Queue::new());

async fn enqueue_sensor_data(mdata: SensorData) {
    let mut p = SENSOR_QUE.lock().await;
    match p.enqueue(mdata) {
        Ok(_) => {}
        Err(el) => {
            p.dequeue().unwrap();
            p.enqueue(el).unwrap();
        }
    }
}

pub struct SensorPeripherals<'a> {
    // SPI (BMP390)
    pub spi2: SPI2<'a>,
    pub spi_clk: GPIO7<'a>,
    pub spi_mosi: GPIO6<'a>,
    pub spi_miso: GPIO9<'a>,
    pub spi_cs: GPIO10<'a>,
    // I2C (SCD41)
    pub i2c0: I2C0<'a>,
    pub i2c_sda: GPIO1<'a>,
    pub i2c_scl: GPIO2<'a>,
    // RGB LED (LEDC PWM)
    pub led: crate::led::RgbLed<'a>,
}

#[embassy_executor::task]
pub async fn sensor_loop(mut p: SensorPeripherals<'static>) {
    // --- тест RGB LED при старте ---
    p.led.startup_test();

    // --- init BMP390 ---
    let spi_bus = crate::spi_helper::init_spi_bus(BarometerArgs {
        spi2: p.spi2,
        clk: p.spi_clk,
        mosi: p.spi_mosi,
        miso: p.spi_miso,
    });
    let mut barometer = get_barometer_spi(spi_bus, p.spi_cs).await;
    let _data = barometer.read_sensor_data().await.unwrap();
    println!("BMP390: init ok");

    // --- init SCD41 ---
    let i2c = I2c::new(
        p.i2c0,
        I2cConfig::default().with_frequency(Rate::from_khz(100)),
    )
    .unwrap()
    .with_sda(p.i2c_sda)
    .with_scl(p.i2c_scl)
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

    // --- калибровка temperature offset по BMP390 ---
    // делаем одно измерение SCD41, сравниваем с BMP390, корректируем offset
    {
        // запускаем single-shot для калибровочного замера
        if let Err(e) = scd.measure_single_shot().await {
            println!("SCD41 cal: single shot error: {:?}", e);
        } else {
            // ждём данные
            loop {
                Timer::after(Duration::from_secs(1)).await;
                match scd.data_ready().await {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(_) => break,
                }
            }
            if let Ok(m) = scd.read_measurement().await {
                // читаем BMP390 температуру (может быть не ready — тогда ждём)
                let mut bmp_temp = None;
                for _ in 0..10 {
                    let status = barometer.read::<IntStatus>().await.unwrap();
                    if status.drdy {
                        let data = barometer.read_sensor_data().await.unwrap();
                        bmp_temp = Some(data.temperature());
                        break;
                    }
                    Timer::after(Duration::from_secs(2)).await;
                }
                if let Some(t_bmp) = bmp_temp {
                    let offset_old = scd.get_temperature_offset().await.unwrap_or(4.0);
                    let offset_new = m.temperature - t_bmp + offset_old;
                    // offset не может быть отрицательным (ограничение датчика)
                    let offset_new = if offset_new < 0.0 { 0.0 } else { offset_new };
                    println!(
                        "SCD41 cal: T_scd={:.2} T_bmp={:.2} offset {:.2} -> {:.2}",
                        m.temperature, t_bmp, offset_old, offset_new
                    );
                    let _ = scd.set_temperature_offset(offset_new).await;
                } else {
                    println!("SCD41 cal: BMP390 not ready, skipping temp offset");
                }
            }
        }
    }

    // --- проверяем ASC ---
    match scd.get_automatic_self_calibration().await {
        Ok(enabled) => println!("SCD41: ASC enabled={}", enabled),
        Err(e) => println!("SCD41: ASC read error: {:?}", e),
    }

    // --- wait NTP ---
    let mut ntp_ready_receiver = CLOCK_IS_SYNCED_WATCH.receiver().unwrap();
    ntp_ready_receiver.changed_and(|s| *s == true).await;

    // --- main loop ~30 сек ---
    let mut last_pressure_hpa: Option<u16> = None;
    loop {
        // 1) читаем BMP390 если готов
        let mut pressure = None;
        let mut baro_temp = None;
        let status = barometer.read::<IntStatus>().await.unwrap();
        if status.drdy {
            let data = barometer.read_sensor_data().await.unwrap();
            pressure = Some(data.pressure());
            baro_temp = Some(data.temperature());
            last_pressure_hpa = Some((data.pressure() / 100.0) as u16);
            println!("BMP390: P={:.1} T={:.2}", data.pressure(), data.temperature());
        }

        // 2) скармливаем давление в SCD41 для компенсации CO2
        if let Some(p_hpa) = last_pressure_hpa {
            let _ = scd.set_ambient_pressure(p_hpa).await;
        }

        // 3) запускаем single-shot SCD41
        if let Err(e) = scd.measure_single_shot().await {
            println!("SCD41: single shot error: {:?}", e);
            Timer::after(Duration::from_secs(30)).await;
            continue;
        }

        // 4) ждём data_ready от SCD41
        loop {
            Timer::after(Duration::from_secs(1)).await;
            match scd.data_ready().await {
                Ok(true) => break,
                Ok(false) => continue,
                Err(e) => {
                    println!("SCD41: data_ready error: {:?}", e);
                    break;
                }
            }
        }

        // 5) читаем SCD41
        let mut co2 = None;
        let mut humidity = None;
        let mut scd_temp = None;
        match scd.read_measurement().await {
            Ok(m) => {
                co2 = Some(m.co2);
                humidity = Some(m.humidity);
                scd_temp = Some(m.temperature);
                println!("SCD41: CO2={} T={:.2} H={:.2}", m.co2, m.temperature, m.humidity);
            }
            Err(e) => {
                println!("SCD41: read error: {:?}", e);
            }
        }

        // 6) объединяем с одним timestamp
        let time = get_current_time_epoch();
        let mdata = SensorData {
            pressure,
            temp: baro_temp,
            co2,
            humidity,
            scd_temp,
            time,
        };
        enqueue_sensor_data(mdata).await;

        // 7) обновляем RGB LED по уровню CO2
        if let Some(co2_val) = co2 {
            p.led.set_co2(co2_val);
        }

        // 8) спим оставшееся время до ~30 сек (уже потратили ~5 на SCD41)
        Timer::after(Duration::from_secs(25)).await;
    }
}
