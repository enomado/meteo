use aes_gcm::aead::AeadInOut;
use aes_gcm::{
    Aes128Gcm,
    KeyInit,
    Nonce,
};
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Runner,
    Stack,
};
use embassy_time::{
    Duration,
    Timer,
};
use esp_println::println;
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig,
    Config,
    Interface,
    WifiController,
};

use crate::led::{
    SYS_NO_TCP,
    SYS_NO_WIFI,
    clear_status,
    set_status,
};
use crate::sensor::{
    SENSOR_QUE,
    SensorData,
};

// not the real crypto, because of reuse nonce!

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

/// Сколько AP забираем из скана. Скан отдаёт список, отсортированный по RSSI;
/// 20 с запасом покрывает и людное окружение — нам нужны лишь свои SSID.
const SCAN_MAX_APS: usize = 20;

/// Выбирает сеть из `WIFI_NETWORKS` с самым сильным сигналом.
///
/// Одна сеть в конфиге — выбирать не из чего, скан пропускаем (это ~2с радио на
/// каждый реконнект). Если скан упал или ни одного своего SSID в эфире нет —
/// ПЕРЕБИРАЕМ сети по кругу (`attempt`), а не липнем к первой: при сломанном
/// скане иначе вторая сеть не была бы испробована никогда.
async fn pick_network(
    controller: &mut WifiController<'static>,
    attempt: usize,
) -> (&'static str, &'static str) {
    let fallback = WIFI_NETWORKS[attempt % WIFI_NETWORKS.len()];

    if WIFI_NETWORKS.len() < 2 {
        return fallback;
    }

    let scan_config = ScanConfig::default().with_max(SCAN_MAX_APS);
    let aps = match controller.scan_async(&scan_config).await {
        Ok(aps) => aps,
        Err(e) => {
            println!("scan failed: {:?}, trying {}", e, fallback.0);
            return fallback;
        }
    };

    let mut best: Option<((&'static str, &'static str), i8)> = None;
    for ap in aps.iter() {
        let Some(net) = WIFI_NETWORKS.iter().find(|(ssid, _)| *ssid == ap.ssid.as_str()) else {
            continue;
        };
        if best.is_none_or(|(_, rssi)| ap.signal_strength > rssi) {
            best = Some((*net, ap.signal_strength));
        }
    }

    match best {
        Some((net, rssi)) => {
            println!("scan: picked {} (rssi {})", net.0, rssi);
            net
        }
        None => {
            println!("scan: no configured SSID on air, trying {}", fallback.0);
            fallback
        }
    }
}

/// Конфиг станции для пары (ssid, пароль) из `WIFI_NETWORKS`.
///
/// `Wpa2Personal` — ровно то, что esp-radio ставил по умолчанию, пока пароль и
/// метод аутентификации были отдельными полями `StationConfig`.
/// Длины SSID/пароля проверяет build-скрипт (`build_helpers::check_network`),
/// поэтому конверсия здесь не может упасть: паника на буте свалила бы плату в
/// бесконечный цикл reset'ов.
pub fn station_config(ssid: &str, passwd: &str) -> Config {
    Config::Station(
        StationConfig::default()
            .with_ssid(ssid.try_into().expect("SSID length checked at build time"))
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                passwd.try_into().expect("password length checked at build time"),
            )),
    )
}

#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    let mut attempt = 0usize;
    loop {
        println!("About to connect...");

        let (ssid, passwd) = pick_network(&mut controller, attempt).await;
        attempt = attempt.wrapping_add(1);

        if let Err(e) = controller.set_config(&station_config(ssid, passwd)) {
            // Смена конфига не удалась — не фатально: коннектимся с тем, что уже
            // стоит в контроллере (в худшем случае это основная сеть).
            println!("set_config({}) failed: {:?}", ssid, e);
        }

        match controller.connect_async().await {
            Ok(info) => {
                println!("Wifi connected to {:?}", info);
                clear_status(SYS_NO_WIFI);
                let info = controller.wait_for_disconnect_async().await.ok();
                println!("Disconnected: {:?}", info);
                set_status(SYS_NO_WIFI);
            }
            Err(e) => {
                println!("Failed to connect to wifi: {:?}", e);
                set_status(SYS_NO_WIFI);
            }
        }

        Timer::after(Duration::from_millis(5000)).await
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

/// Ёмкость батча в памяти. Заполняется не полностью — фактический размер режет
/// `MAX_BATCH`; запас нужен, чтобы буфер не пришлось трогать при правке батча.
const BATCH_CAP: usize = 40;

/// Батч показаний между очередью сенсора и сокетом. Алиас, потому что ёмкость
/// раньше повторялась литералом в трёх сигнатурах и разъезжалась при правке.
type SensorBatch = heapless::Vec<SensorData, BATCH_CAP>;

/// Максимум записей в один пакет. Бюджет postcard в write_packet = 1004 байта
/// (body_buf[4..1024-16]); одна SensorData ≤ ~32 байт (baro 9 + scd 13 + time 10
/// varint). 24 × 32 + 1 = 769 < 1004 — с большим запасом, overflow невозможен.
/// Остаток очереди (до 60) дренится следующими пакетами (каждые ~3с).
const MAX_BATCH: usize = 24;

async fn get_sensor_data_chunk() -> SensorBatch {
    let mut out = SensorBatch::new();
    let Ok(mut p) = SENSOR_QUE.try_lock() else {
        return out;
    };

    // Cap на MAX_BATCH: НЕ пихаем весь backlog в один пакет — иначе postcard
    // переполнит фикс-буфер (было: паника через .unwrap() → заморозка чипа,
    // инцидент 2026-07-04). Дренаж backlog'а — за несколько пакетов.
    while out.len() < MAX_BATCH {
        let Some(v) = p.dequeue() else {
            break;
        };
        if out.push(v).is_err() {
            break;
        }
    }

    out
}

#[embassy_executor::task]
pub async fn network_send_loop(stack: Stack<'static>) {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 2048];

    stack.wait_link_up().await;
    stack.wait_config_up().await;

    // 188.245.58.248
    // let remote_endpoint = (Ipv4Addr::new(188, 245, 58, 248), 1234);
    let remote_endpoint = (SERVER_IP, SERVER_PORT);

    let mut measurements_buf = SensorBatch::new();

    loop {
        // heartbeat для watchdog (внешний цикл: реконнект). Бьётся даже когда
        // сервер недоступен — retry ≤5с, что watchdog'ом НЕ считается зависанием.
        crate::watchdog::beat_net();

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(120)));

        println!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            set_status(SYS_NO_TCP);
            Timer::after(Duration::from_millis(5000)).await;
            continue;
        }

        println!("connected!");
        clear_status(SYS_NO_TCP);

        // Счётчик nonce живёт внутри соединения: сервер считает пакеты с 1 на
        // каждый accept (см. example_server), поэтому обнуляем на реконнекте.
        let mut nonce_counter = 0u64;

        loop {
            // heartbeat для watchdog (внутренний цикл: send). ≤3с при данных,
            // ≤1с при пустой очереди.
            crate::watchdog::beat_net();

            if measurements_buf.is_empty() {
                measurements_buf = get_sensor_data_chunk().await
            }

            let p = &measurements_buf;

            if p.is_empty() {
                Timer::after(Duration::from_millis(1000)).await;
                continue;
            }

            nonce_counter += 1;
            println!("sending {} measurements, nonce={}", p.len(), nonce_counter);
            let r = write_packet(&mut socket, p, nonce_counter).await;

            match r {
                Ok(g) => {
                    println!("write ok, {} bytes", g);
                    measurements_buf.clear();
                }
                Err(SendError::Serialize) => {
                    // Батч не влезает в буфер. При MAX_BATCH недостижимо, но НЕ
                    // паникуем (было: .unwrap() → заморозка чипа). Дропаем батч,
                    // чтобы не застрять в вечном ретрае одного пакета.
                    println!("serialize error: batch too big, dropping {} readings", p.len());
                    measurements_buf.clear();
                }
                Err(SendError::Tcp(e)) => {
                    println!("write error: {:?}", e);
                    set_status(SYS_NO_TCP);
                    Timer::after(Duration::from_millis(3000)).await;
                    break;
                }
            }

            Timer::after(Duration::from_millis(3000)).await;
        }

        Timer::after(Duration::from_millis(3000)).await;
    }
}

/// Ошибка отправки пакета. Разделяем сериализацию и транспорт: overflow буфера
/// — НЕ повод рвать соединение (и тем более паниковать), а TCP-ошибка — повод
/// реконнекта.
enum SendError {
    /// postcard не влез в фикс-буфер (батч слишком большой). При MAX_BATCH
    /// недостижимо; пришло на смену `.unwrap()`, который морозил чип.
    Serialize,
    /// Ошибка записи в сокет — рвём и реконнектимся.
    Tcp(embassy_net::tcp::Error),
}

/// On-wire layout: `[u32 BE payload_len][AES-GCM ciphertext][16-byte tag]`
/// где `payload_len` = ciphertext_len + 16 (tag inline).
/// Шифруем in-place в `body_buf[4..]`, tag дописываем сразу после — без heap-Vec.
async fn write_packet(
    socket: &mut TcpSocket<'_>,
    p: &SensorBatch,
    nonce_counter: u64,
) -> Result<usize, SendError> {
    /// Длина префикса `u32 BE payload_len` перед шифротекстом.
    const LEN_PREFIX: usize = 4;
    const TAG_LEN: usize = 16;
    const BUF_LEN: usize = 1024;
    let mut body_buf = [0u8; BUF_LEN];

    // postcard в body_buf[LEN_PREFIX..], оставив запас под tag в конце. Overflow
    // → Err (НЕ паника): при MAX_BATCH недостижимо, но fail-safe важнее — паника
    // здесь морозила чип навсегда (инцидент 2026-07-04).
    let body = &mut body_buf[LEN_PREFIX..BUF_LEN - TAG_LEN];
    let plain_len = match postcard::to_slice(p.as_slice(), body) {
        Ok(s) => s.len(),
        Err(_) => return Err(SendError::Serialize),
    };

    let cipher = Aes128Gcm::new_from_slice(&SECRET_KEY).unwrap();

    // nonce = 96 бит: 4 байта паддинга + 8 байт BE-counter. ВНИМАНИЕ: reuse при
    // рестарте MCU (counter сбрасывается) — это известная (намеренная) дыра.
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[4..].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from(nonce_bytes);

    // шифрование in-place (InOutBuf поверх среза), tag отдельно
    let plain_end = LEN_PREFIX + plain_len;
    let tag = cipher
        .encrypt_inout_detached(&nonce, b"", (&mut body_buf[LEN_PREFIX..plain_end]).into())
        .unwrap();
    body_buf[plain_end..plain_end + TAG_LEN].copy_from_slice(&tag);

    let payload_len = plain_len + TAG_LEN;
    body_buf[..LEN_PREFIX].copy_from_slice(&(payload_len as u32).to_be_bytes());

    let total_len = LEN_PREFIX + payload_len;
    socket.write(&body_buf[..total_len]).await.map_err(SendError::Tcp)
}
