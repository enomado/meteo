use bmp390_rs::register::int_status::IntStatus;
use bmp390_rs::{
    Bmp390,
    ResetPolicy,
};
use embassy_sync::blocking_mutex::raw::{
    CriticalSectionRawMutex,
    NoopRawMutex,
};
use embassy_sync::mutex::Mutex;
use embassy_time::{
    Delay,
    Duration,
    Timer,
};
use esp_hal::gpio::{
    Level,
    Output,
    OutputConfig,
};
use esp_hal::i2c::master::{
    Config as I2cConfig,
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
use heapless::spsc::Queue;
use libscd::asynchronous::scd4x::Scd4x;

use crate::led::{
    SYS_BUF_OVERFLOW,
    SYS_NO_PERIPH,
    clear_co2,
    clear_status,
    publish_co2,
    set_status,
};
use crate::ntp_client::{
    CLOCK_IS_SYNCED_WATCH,
    EpochMillis,
    now_epoch,
};
use crate::spi_helper::BarometerArgs;

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

/// Один замер SCD41: single-shot → ожидание data_ready → чтение.
/// `None` — замер в этом цикле не удался (причина уже в логе); вызывающий
/// гасит CO2 на LED и поднимает `SYS_NO_PERIPH`.
async fn measure_scd(scd: &mut ScdDevice<'_>) -> Option<ScdReading> {
    if let Err(e) = scd.measure_single_shot().await {
        println!("SCD41: single shot error: {:?}", e);
        return None;
    }

    wait_scd_ready(scd).await;

    match scd.read_measurement().await {
        Ok(m) => {
            println!("SCD41: CO2={} T={:.2} H={:.2}", m.co2, m.temperature, m.humidity);
            Some(ScdReading {
                co2:      m.co2,
                humidity: m.humidity,
                temp:     m.temperature,
            })
        }
        Err(e) => {
            println!("SCD41: read error: {:?}", e);
            None
        }
    }
}

/// Результат одного опроса BMP390. «Ещё не готов» — штатное состояние (ODR
/// ~0.05 Гц), а не ошибка: реакция на них разная, поэтому состояния разведены.
enum BaroPoll {
    Ready(BaroReading),
    NotReady,
    /// Ошибка шины. НЕ паникуем (было: `.unwrap()` → reset всего чипа на
    /// транзиенте SPI) — вызывающий решает, ретраить или пропустить цикл.
    Failed,
}

/// Один опрос BMP390: статус drdy → чтение. Ошибки шины логируются здесь,
/// решение (ретрай / пропуск цикла / SYS_NO_PERIPH) принимает вызывающий.
async fn poll_barometer(barometer: &mut BarometerDevice<'_>) -> BaroPoll {
    match barometer.read::<IntStatus>().await {
        Ok(status) if !status.drdy => BaroPoll::NotReady,
        Ok(_) => {
            match barometer.read_sensor_data().await {
                Ok(data) => {
                    BaroPoll::Ready(BaroReading {
                        pressure: data.pressure(),
                        temp:     data.temperature(),
                    })
                }
                Err(e) => {
                    println!("BMP390: read_sensor_data error: {:?}", e);
                    BaroPoll::Failed
                }
            }
        }
        Err(e) => {
            println!("BMP390: status read error: {:?}", e);
            BaroPoll::Failed
        }
    }
}

/// Калибровка SCD41 temperature offset по показанию BMP390.
/// Делает single-shot SCD41, ждёт drdy на барометре, считает дельту и пишет новый offset.
/// Если барометра нет / он не отдал данные — offset не трогаем.
async fn calibrate_temp_offset(scd: &mut ScdDevice<'_>, barometer: Option<&mut BarometerDevice<'_>>) {
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

    // BMP390 температура — до 10 попыток по 2 сек. Ошибку шины НЕ паникуем и НЕ
    // сдаёмся сразу: транзиент шины может пройти → ретраим в пределах того же
    // бюджета попыток (как и ожидание drdy). Жёсткий предел остаётся: калибровка
    // на старте, блокировать её навсегда нельзя — иначе не пойдут CO2-данные.
    let mut bmp_temp = None;
    if let Some(barometer) = barometer {
        for _ in 0..10 {
            if let BaroPoll::Ready(reading) = poll_barometer(barometer).await {
                bmp_temp = Some(reading.temp);
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
    pub temp:     f32,
}

/// SCD41 одно показание — CO2 (ppm), humidity (%), temperature (°C).
/// Поля всегда заполнены или отсутствуют синхронно (один read_measurement).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ScdReading {
    pub co2:      u16,
    pub humidity: f32,
    pub temp:     f32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SensorData {
    pub baro: Option<BaroReading>,
    pub scd:  Option<ScdReading>,
    pub time: EpochMillis,
}

pub static SENSOR_QUE: Mutex<CriticalSectionRawMutex, Queue<SensorData, 60>> = Mutex::new(Queue::new());

async fn enqueue_sensor_data(mdata: SensorData) {
    let mut p = SENSOR_QUE.lock().await;
    match p.enqueue(mdata) {
        Ok(_) => {
            clear_status(SYS_BUF_OVERFLOW);
        }
        Err(el) => {
            // Очередь полна ⇒ в ней есть хотя бы один элемент, и после dequeue
            // ровно одно место свободно: оба вызова не могут не сработать.
            p.dequeue().expect("full queue has at least one entry");
            p.enqueue(el).expect("dequeue freed exactly one slot");
            set_status(SYS_BUF_OVERFLOW);
        }
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
    let i2c = I2c::new(i2c0, I2cConfig::default().with_frequency(Rate::from_khz(100)))
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
            println!("SCD41: failed to read serial: {:?}, sensor not connected?", e);
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
        // heartbeat для watchdog: доказывает что таска крутит свой цикл, а не
        // зависла на I2C/SPI-await. Раз в ~30с.
        crate::watchdog::beat_sensor();

        // 1) читаем BMP390 если готов. Ошибку шины НЕ паникуем (было: .unwrap()
        // → reset всего чипа на транзиенте SPI): пропускаем давление в этом
        // цикле, сбой учитываем в SYS_NO_PERIPH в конце итерации.
        let mut baro: Option<BaroReading> = None;
        let mut baro_failed = false;
        if let Some(ref mut barometer) = barometer {
            match poll_barometer(barometer).await {
                BaroPoll::Ready(reading) => {
                    last_pressure_hpa = Some((reading.pressure / 100.0) as u16);
                    println!("BMP390: P={:.1} T={:.2}", reading.pressure, reading.temp);
                    baro = Some(reading);
                }
                BaroPoll::NotReady => {} // не drdy — штатно, ждём следующий цикл
                BaroPoll::Failed => baro_failed = true,
            }
        }

        // 2) скармливаем давление в SCD41 для компенсации CO2
        if let Some(p_hpa) = last_pressure_hpa {
            let _ = scd.set_ambient_pressure(p_hpa).await;
        }

        // 3) single-shot SCD41 → data_ready → чтение
        let scd_reading = measure_scd(&mut scd).await;

        // 4) CO2 для LED-таска — ВСЕГДА, даже без сети и без NTP-времени. Сбой
        // SCD41 гасит показание: старое значение на LED выдавало бы мёртвый
        // сенсор за живой.
        match &scd_reading {
            Some(s) => publish_co2(s.co2),
            None => clear_co2(),
        }

        // 5) SYS_NO_PERIPH — ОДИН писатель, одно вычисление на итерацию. Раньше
        // сбой BMP390 ставил бит, а успешное чтение SCD41 в той же итерации его
        // снимало ⇒ сбой барометра на LED не был виден никогда.
        if barometer.is_none() || baro_failed || scd_reading.is_none() {
            set_status(SYS_NO_PERIPH);
        } else {
            clear_status(SYS_NO_PERIPH);
        }

        // 6) в серверный буфер кладём только с доверенным временем (после первого
        // NTP-синка). try_get неблокирующий; once-synced остаётся Some(true) и при
        // последующих кратких обрывах WiFi — буферизация продолжается. Холодный
        // старт без WiFi → не засоряем очередь baked-in таймстампами.
        if ntp_ready_receiver.try_get() == Some(true) {
            let mdata = SensorData {
                baro,
                scd: scd_reading,
                time: now_epoch(),
            };
            enqueue_sensor_data(mdata).await;
        }

        // 7) спим оставшееся время до ~30 сек (уже потратили ~5 на SCD41)
        Timer::after(Duration::from_secs(25)).await;
    }
}
