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
    led::{SYS_BUF_OVERFLOW, SYS_NO_PERIPH, clear_status, publish_co2, set_status},
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

pub type ScdDevice<'a> = Scd4x<I2c<'a, esp_hal::Async>, Delay>;

/// Поллит `data_ready` раз в секунду до Ok(true).
/// Возвращает `false`, если поллинг был прерван ошибкой шины.
async fn wait_scd_ready(scd: &mut ScdDevice<'_>) -> bool {
    loop {
        Timer::after(Duration::from_secs(1)).await;
        match scd.data_ready().await {
            Ok(true) => return true,
            Ok(false) => continue,
            Err(e) => {
                println!("SCD41: data_ready error: {:?}", e);
                return false;
            }
        }
    }
}

/// Калибровка SCD41 temperature offset по показанию BMP390.
/// Делает single-shot SCD41, ждёт drdy на барометре, считает дельту и пишет новый offset.
/// Если барометра нет / он не отдал данные — offset не трогаем.
async fn calibrate_temp_offset(
    scd: &mut ScdDevice<'_>,
    barometer: Option<&mut BarometerDevice<'_>>,
) {
    if let Err(e) = scd.measure_single_shot().await {
        println!("SCD41 cal: single shot error: {:?}", e);
        return;
    }
    if !wait_scd_ready(scd).await {
        return;
    }
    let Ok(m) = scd.read_measurement().await else {
        return;
    };

    // BMP390 температура — ждём до 10 попыток по 2 сек.
    let mut bmp_temp = None;
    if let Some(barometer) = barometer {
        for _ in 0..10 {
            let status = barometer.read::<IntStatus>().await.unwrap();
            if status.drdy {
                let data = barometer.read_sensor_data().await.unwrap();
                bmp_temp = Some(data.temperature());
                break;
            }
            Timer::after(Duration::from_secs(2)).await;
        }
    }

    let Some(t_bmp) = bmp_temp else {
        println!("SCD41 cal: BMP390 not ready, skipping temp offset");
        return;
    };

    let offset_old = scd.get_temperature_offset().await.unwrap_or(4.0);
    // offset не может быть отрицательным (ограничение датчика).
    let offset_new = (m.temperature - t_bmp + offset_old).max(0.0);
    println!(
        "SCD41 cal: T_scd={:.2} T_bmp={:.2} offset {:.2} -> {:.2}",
        m.temperature, t_bmp, offset_old, offset_new
    );
    let _ = scd.set_temperature_offset(offset_new).await;
}

pub async fn get_barometer_spi<'a>(
    spi_bus: &'a Mutex<NoopRawMutex, Spi<'a, esp_hal::Async>>,
    cs_pin: GPIO10<'a>,
) -> Option<BarometerDevice<'a>> {
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

    Timer::after(Duration::from_millis(150)).await;

    match Bmp390::new_spi(spi_device, config, ResetPolicy::None, &mut delay).await {
        Ok(device) => Some(device),
        Err(err) => {
            println!("BMP390: init failed: {:?}", err);
            println!("BMP390: continuing without barometer");
            None
        }
    }
}

/// BMP390 одно показание — pressure (Pa) + temperature (°C). Поля всегда заполнены
/// или отсутствуют синхронно (читаются одним вызовом read_sensor_data).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct BaroReading {
    pub pressure: f32,
    pub temp: f32,
}

/// SCD41 одно показание — CO2 (ppm), humidity (%), temperature (°C).
/// Поля всегда заполнены или отсутствуют синхронно (один read_measurement).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ScdReading {
    pub co2: u16,
    pub humidity: f32,
    pub temp: f32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SensorData {
    pub baro: Option<BaroReading>,
    pub scd: Option<ScdReading>,
    /// millis epoch
    pub time: u64,
}

pub static SENSOR_QUE: Mutex<CriticalSectionRawMutex, Queue<SensorData, 60>> =
    Mutex::new(Queue::new());

async fn enqueue_sensor_data(mdata: SensorData) {
    let mut p = SENSOR_QUE.lock().await;
    match p.enqueue(mdata) {
        Ok(_) => {
            clear_status(SYS_BUF_OVERFLOW);
        }
        Err(el) => {
            p.dequeue().unwrap();
            p.enqueue(el).unwrap();
            set_status(SYS_BUF_OVERFLOW);
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
}

#[embassy_executor::task]
pub async fn sensor_loop(p: SensorPeripherals<'static>) {
    let SensorPeripherals {
        spi2,
        spi_clk,
        spi_mosi,
        spi_miso,
        spi_cs,
        i2c0,
        i2c_sda,
        i2c_scl,
    } = p;

    // --- init BMP390 ---
    let spi_bus = crate::spi_helper::init_spi_bus(BarometerArgs {
        spi2,
        clk: spi_clk,
        mosi: spi_mosi,
        miso: spi_miso,
    });
    let mut barometer = get_barometer_spi(spi_bus, spi_cs).await;
    if barometer.is_some() {
        println!("BMP390: init ok");
    } else {
        set_status(SYS_NO_PERIPH);
    }

    // --- init SCD41 ---
    let i2c = I2c::new(
        i2c0,
        I2cConfig::default().with_frequency(Rate::from_khz(100)),
    )
    .unwrap()
    .with_sda(i2c_sda)
    .with_scl(i2c_scl)
    .into_async();

    let mut scd = Scd4x::new(i2c, Delay);

    // остановить на случай если датчик уже измерял (после перезагрузки MCU)
    let _ = scd.stop_periodic_measurement().await;
    Timer::after(Duration::from_millis(500)).await;

    let serial = scd.serial_number().await;
    match serial {
        Ok(s) => println!("SCD41 serial: {:?}", s),
        Err(e) => {
            println!(
                "SCD41: failed to read serial: {:?}, sensor not connected?",
                e
            );
            set_status(SYS_NO_PERIPH);
            return;
        }
    }

    calibrate_temp_offset(&mut scd, barometer.as_mut()).await;

    // --- проверяем ASC ---
    match scd.get_automatic_self_calibration().await {
        Ok(enabled) => println!("SCD41: ASC enabled={}", enabled),
        Err(e) => println!("SCD41: ASC read error: {:?}", e),
    }

    // NTP-синк больше НЕ блокирует измерения: CO2/LED должны работать и без сети
    // (комната без WiFi). Раньше sensor_loop висел на этом await навсегда, поэтому
    // publish_co2 не вызывался, has_co2 в led_loop оставался false, и индикатор
    // сваливался в безостановочный blink-цикл вместо штатного оверлея раз/мин.
    // Флаг синка теперь читаем неблокирующе (try_get) — только чтобы не класть в
    // серверный буфер показания с дефолтным (неверным) таймстампом до первого NTP.
    let mut ntp_ready_receiver = CLOCK_IS_SYNCED_WATCH.receiver().unwrap();

    // --- main loop ~30 сек ---
    let mut last_pressure_hpa: Option<u16> = None;
    loop {
        // 1) читаем BMP390 если готов
        let mut baro: Option<BaroReading> = None;
        if let Some(ref mut barometer) = barometer {
            let status = barometer.read::<IntStatus>().await.unwrap();
            if status.drdy {
                let data = barometer.read_sensor_data().await.unwrap();
                let pressure = data.pressure();
                let temp = data.temperature();
                last_pressure_hpa = Some((pressure / 100.0) as u16);
                println!("BMP390: P={:.1} T={:.2}", pressure, temp);
                baro = Some(BaroReading { pressure, temp });
            }
        }

        // 2) скармливаем давление в SCD41 для компенсации CO2
        if let Some(p_hpa) = last_pressure_hpa {
            let _ = scd.set_ambient_pressure(p_hpa).await;
        }

        // 3) запускаем single-shot SCD41
        if let Err(e) = scd.measure_single_shot().await {
            println!("SCD41: single shot error: {:?}", e);
            set_status(SYS_NO_PERIPH);
            Timer::after(Duration::from_secs(30)).await;
            continue;
        }

        // 4) ждём data_ready от SCD41
        wait_scd_ready(&mut scd).await;

        // 5) читаем SCD41
        let mut scd_reading: Option<ScdReading> = None;
        match scd.read_measurement().await {
            Ok(m) => {
                println!(
                    "SCD41: CO2={} T={:.2} H={:.2}",
                    m.co2, m.temperature, m.humidity
                );
                // если barometer был None при init — бит остаётся
                if barometer.is_some() {
                    clear_status(SYS_NO_PERIPH);
                }
                scd_reading = Some(ScdReading {
                    co2: m.co2,
                    humidity: m.humidity,
                    temp: m.temperature,
                });
            }
            Err(e) => {
                println!("SCD41: read error: {:?}", e);
                set_status(SYS_NO_PERIPH);
            }
        }

        // 6) публикуем CO2 для LED-таска — ВСЕГДА, даже без сети и без NTP-времени.
        let co2_for_led = scd_reading.as_ref().map(|s| s.co2);
        if let Some(c) = co2_for_led {
            publish_co2(c);
        }

        // 7) в серверный буфер кладём только с доверенным временем (после первого
        // NTP-синка). try_get неблокирующий; once-synced остаётся Some(true) и при
        // последующих кратких обрывах WiFi — буферизация продолжается. Холодный
        // старт без WiFi → не засоряем очередь baked-in таймстампами.
        if ntp_ready_receiver.try_get() == Some(true) {
            let mdata = SensorData {
                baro,
                scd: scd_reading,
                time: get_current_time_epoch(),
            };
            enqueue_sensor_data(mdata).await;
        }

        // 8) спим оставшееся время до ~30 сек (уже потратили ~5 на SCD41)
        Timer::after(Duration::from_secs(25)).await;
    }
}
