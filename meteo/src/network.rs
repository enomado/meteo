use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use embassy_net::tcp::{
    Error as TcpError,
    State,
    TcpSocket,
};
use embassy_net::{
    Runner,
    Stack,
};
use embassy_time::{
    Duration,
    TimeoutError,
    Timer,
    with_timeout,
};
use embedded_io_async::Write;
use esp_println::println;
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig,
    Config,
    Interface,
    WifiController,
};
use meteo_core::wire::{
    PacketBuf,
    PacketCounter,
    SensorBatch,
    encode_packet,
};

use crate::led::{
    SYS_NO_TCP,
    SYS_NO_WIFI,
    clear_status,
    set_status,
};
use crate::sensor::SENSOR_QUE;

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

async fn get_sensor_data_chunk() -> SensorBatch {
    let mut out = SensorBatch::new();
    let Ok(mut p) = SENSOR_QUE.try_lock() else {
        return out;
    };

    // НЕ пихаем весь backlog в один пакет: он не влез бы в буфер (ёмкость
    // батча = `MAX_BATCH`). Дренаж backlog'а — за несколько пакетов.
    while !out.is_full() {
        let Some(v) = p.dequeue() else {
            break;
        };
        out.push(v).expect("loop runs only while the batch has room");
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

    // Ключ постоянный ⇒ key schedule AES разворачиваем один раз на таску, а не
    // на каждый пакет. `new` от массива фиксированной длины упасть не может
    // (было: `new_from_slice(..).unwrap()` на каждом пакете).
    let cipher = Aes128Gcm::new(&SECRET_KEY.into());

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

        // Счётчик пакетов (из него nonce) живёт внутри соединения: сервер
        // считает пакеты с 1 на каждый accept, поэтому обнуляем на реконнекте.
        let mut counter = PacketCounter::FIRST;

        loop {
            // heartbeat для watchdog (внутренний цикл: send). ≤3с при данных
            // (+ до SEND_TIMEOUT на ожидание ACK), ≤1с при пустой очереди.
            crate::watchdog::beat_net();

            if measurements_buf.is_empty() {
                measurements_buf = get_sensor_data_chunk().await
            }

            let p = &measurements_buf;

            if p.is_empty() {
                Timer::after(Duration::from_millis(1000)).await;
                continue;
            }

            println!("sending {} measurements, packet #{}", p.len(), counter.0);
            let r = write_packet(&mut socket, &cipher, p, counter).await;
            counter = counter.next();

            match r {
                Ok(()) => {
                    measurements_buf.clear();
                }
                Err(SendError::Tcp(e)) => {
                    // Батч НЕ чистим: без ACK неизвестно, дошёл ли он. Повтор на
                    // новом соединении безопасен — приёмник идемпотентен по времени.
                    println!("write error: {:?}", e);
                    set_status(SYS_NO_TCP);
                    Timer::after(Duration::from_millis(3000)).await;
                    break;
                }
                Err(SendError::Timeout) => {
                    println!("send timeout: no ACK in {}s", SEND_TIMEOUT.as_secs());
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

/// Ошибка отправки пакета — любая ведёт к реконнекту. Переполнения буфера
/// среди причин нет: его исключает проверка бюджета пакета при компиляции.
enum SendError {
    /// Ошибка записи в сокет или соединение закрылось до ACK — рвём и
    /// реконнектимся.
    Tcp(embassy_net::tcp::Error),
    /// Пакет не записан/не подтверждён за `SEND_TIMEOUT` — рвём и реконнектимся.
    Timeout,
}

/// Предел на запись пакета и ожидание ACK. Ожидание в `write_all`/`flush`
/// ограничено только socket-timeout'ом (120с, сбрасывается любым входящим
/// сегментом), и в сумме с соседними ожиданиями могло перерасти
/// `NET_STALL_LIMIT` (180с) ⇒ ресет всего чипа вместо реконнекта.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Пишет пакет целиком и ждёт, пока удалённый TCP подтвердит все байты.
///
/// `write` кладёт в tx-буфер сколько влезло и возвращает это число: при
/// медленных ACK хвост пакета терялся, приёмник терял фрейминг потока —
/// поэтому `write_all`. Успех записи = байты в tx-буфере, не у приёмника —
/// поэтому `flush`. Но `flush` embassy-net 0.9 отдаёт `Ok` и когда сокет ушёл
/// в `Closed` (RST/таймаут) с недоставленными данными: его условие ожидания —
/// `send_queue() > 0 && state() != Closed`. Доставку подтверждает только живое
/// состояние после `flush`.
async fn deliver(socket: &mut TcpSocket<'_>, packet: &[u8]) -> Result<(), SendError> {
    socket.write_all(packet).await.map_err(SendError::Tcp)?;
    socket.flush().await.map_err(SendError::Tcp)?;
    if socket.state() == State::Closed {
        return Err(SendError::Tcp(TcpError::ConnectionReset));
    }
    Ok(())
}

/// Кодирует батч (формат — `meteo_core::wire`) и доставляет его.
async fn write_packet(
    socket: &mut TcpSocket<'_>,
    cipher: &Aes128Gcm,
    p: &SensorBatch,
    counter: PacketCounter,
) -> Result<(), SendError> {
    let mut packet: PacketBuf = [0u8; _];
    let total_len = encode_packet(cipher, counter, p, &mut packet);
    match with_timeout(SEND_TIMEOUT, deliver(socket, &packet[..total_len])).await {
        Ok(delivered) => delivered?,
        Err(TimeoutError) => return Err(SendError::Timeout),
    }
    println!("delivered, {} bytes", total_len);
    Ok(())
}
