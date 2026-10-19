// Wi-Fi and network tasks run on one ESP32-C3 Embassy executor; their futures
// do not migrate between threads, so the hardware drivers may remain !Send.
#![allow(
    clippy::future_not_send,
    reason = "Wi-Fi and network futures run on the single ESP32-C3 Embassy executor"
)]

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use embassy_net::udp::{
    PacketMetadata,
    UdpSocket,
};
use embassy_net::{
    IpEndpoint,
    Runner,
    Stack,
};
use embassy_time::{
    Duration,
    Instant,
    TimeoutError,
    Timer,
    with_deadline,
    with_timeout,
};
use esp_hal::rng::Rng;
use esp_println::println;
use esp_radio::wifi::event::EventInfo;
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig,
    Config,
    ConnectionError,
    Interface,
    WifiController,
    WifiError,
};
use meteo_core::datagram::BootId;
use meteo_core::health::{
    Health,
    NetStage,
    Word,
};
use meteo_core::sender::{
    Backlog,
    Mode,
    Sender,
};
use meteo_core::wifi_pick::{
    Network,
    Rssi,
    ScannedAccessPoint,
    round_robin,
    strongest,
};
use meteo_core::wifi_retry::{
    RETRY_PAUSE,
    Reconnect,
};
use portable_atomic::{
    AtomicU32,
    Ordering,
};
use static_cell::ConstStaticCell;

use crate::led::{
    SYS_BUF_OVERFLOW,
    SYS_NO_SERVER,
    SYS_NO_WIFI,
    clear_status,
    set_status,
};
use crate::sensor::{
    QUEUE_EVICTIONS,
    SENSOR_QUE,
};
use crate::watchdog::{
    health,
    uptime,
};
use crate::wifi_reason::reason_code;

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

/// Сколько AP забираем из скана. Скан отдаёт список, отсортированный по RSSI;
/// 20 с запасом покрывает и людное окружение — нам нужны лишь свои SSID.
const SCAN_MAX_APS: usize = 20;

fn check_network_count() {
    // build_helpers currently produces the primary network and at most one
    // optional secondary network. Keep this assertion beside the fixed buffer
    // so extending the generated configuration cannot silently drop entries.
    assert!(
        (1..=2).contains(&WIFI_NETWORKS.len()),
        "firmware expects one or two configured Wi-Fi networks"
    );
}

/// Основная сеть для начальной конфигурации радио до первого скана.
#[must_use]
pub fn primary_network() -> Network<'static> {
    check_network_count();
    WIFI_NETWORKS[0]
}

/// Выбирает сеть из `WIFI_NETWORKS` с самым сильным сигналом (решение —
/// `meteo_core::wifi_pick`, здесь только скан).
///
/// Одна сеть в конфиге — выбирать не из чего, скан пропускаем (это ~2с радио на
/// каждый реконнект). Если скан упал или ни одного своего SSID в эфире нет —
/// перебираем сети по кругу (`attempt`).
async fn pick_network(controller: &mut WifiController<'static>, attempt: usize) -> Network<'static> {
    check_network_count();
    let first = WIFI_NETWORKS[0];
    let mut configured = [first; 2];
    if let Some(&network) = WIFI_NETWORKS.get(1) {
        configured[1] = network;
    }
    let networks = &configured[..WIFI_NETWORKS.len()];
    let fallback = round_robin(networks, attempt);

    if networks.len() < 2 {
        return fallback;
    }

    let scan_config = ScanConfig::default().with_max(SCAN_MAX_APS);
    let aps = match controller.scan_async(&scan_config).await {
        Ok(aps) => aps,
        Err(e) => {
            println!("scan failed: {:?}, trying {}", e, fallback.ssid);
            return fallback;
        }
    };

    let seen = aps.iter().map(|ap| {
        ScannedAccessPoint {
            ssid: ap.ssid.as_str(),
            rssi: Rssi(ap.signal_strength),
        }
    });
    strongest(networks, seen).map_or_else(
        || {
            println!("scan: no configured SSID on air, trying {}", fallback.ssid);
            fallback
        },
        |pick| {
            println!("scan: picked {} (rssi {})", pick.network.ssid, pick.rssi.0);
            pick.network
        },
    )
}

/// Конфиг станции для сети из `WIFI_NETWORKS`.
///
/// `Wpa2Personal` — ровно то, что esp-radio ставил по умолчанию, пока пароль и
/// метод аутентификации были отдельными полями `StationConfig`.
/// Пароль — готовый PSK (64 hex): supplicant берёт его без PBKDF2, см.
/// `build_helpers::wpa_psk_hex`. Длину SSID проверяет, а PSK выводит
/// build-скрипт, поэтому конверсия здесь не может упасть: паника на буте
/// свалила бы плату в бесконечный цикл reset'ов.
///
/// # Panics
///
/// Panics if the build-time SSID length check or PSK derivation is bypassed.
#[must_use]
pub fn station_config(network: Network<'_>) -> Config {
    Config::Station(
        StationConfig::default()
            .with_ssid(
                network
                    .ssid
                    .try_into()
                    .expect("SSID length checked at build time"),
            )
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                network
                    .psk_hex
                    .try_into()
                    .expect("PSK is 64 hex characters, derived at build time"),
            )),
    )
}

/// Ждёт обрыва Wi-Fi и отдаёт сырой код причины: номер IEEE 802.11 (1–68)
/// или ESP (200+).
///
/// `wait_for_disconnect_async` отдаёт только `DisconnectReason`: неизвестный
/// код там схлопнут в `Unspecified`, а номера из enum апстрим убрал намеренно
/// (esp-hal#4923). Сырой код есть лишь в событии `StationDisconnected`
/// (unstable API), поэтому ждём его сами — так же, как это делает
/// `wait_for_disconnect_async`, но подписываемся ДО проверки `is_connected`:
/// обрыв между проверкой и подпиской не теряется.
async fn wait_for_disconnect_reason(controller: &WifiController<'static>) -> Result<u16, WifiError> {
    // Подписчиков у канала событий два (ESP_RADIO_CONFIG_EVENT_CHANNEL_SUBSCRIBERS);
    // connect/scan этой же таски к этому моменту свою подписку уже отпустили.
    let mut events = controller.subscribe()?;
    if !controller.is_connected() {
        return Err(WifiError::NotConnected);
    }
    loop {
        let event = events.next_event_pure().await;
        if let EventInfo::StationDisconnected { reason, .. } = event {
            println!("Disconnected: {:?}", event);
            return Ok(reason);
        }
    }
}

/// Когда повторять и когда поднимать `SYS_NO_WIFI` — `meteo_core::wifi_retry`.
#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    let mut attempt = 0usize;
    let mut reconnect = Reconnect::new();
    loop {
        println!("About to connect...");

        let network = pick_network(&mut controller, attempt).await;
        attempt = attempt.wrapping_add(1);

        if let Err(e) = controller.set_config(&station_config(network)) {
            // Смена конфига не удалась — не фатально: коннектимся с тем, что уже
            // стоит в контроллере (в худшем случае это основная сеть).
            println!("set_config({}) failed: {:?}", network.ssid, e);
        }

        let pause = match controller.connect_async().await {
            Ok(info) => {
                println!("Wifi connected to {:?}", info);
                reconnect.on_connected();
                clear_status(SYS_NO_WIFI);
                let reason = wait_for_disconnect_reason(&controller).await;
                set_status(SYS_NO_WIFI);
                reconnect.on_dropped();
                let health = health();
                health.bump(Word::WifiDisconnects);
                match reason {
                    Ok(reason) => health.set(Word::WifiLastReason, u32::from(reason)),
                    Err(e) => println!("Disconnected, reason unknown: {:?}", e),
                }
                RETRY_PAUSE
            }
            Err(e) => {
                println!("Failed to connect to wifi: {:?}", e);
                // Причина есть только у отказа точки доступа; `WifiError` —
                // драйвер отказал, не дойдя до неё.
                let reason = match e {
                    ConnectionError::Failed(info) => reason_code(info.reason),
                    _ => None,
                };
                let health = health();
                health.bump(Word::WifiConnectFailures);
                if let Some(reason) = reason {
                    health.set(Word::WifiLastReason, u32::from(reason.0));
                }
                let verdict = reconnect.on_failed(reason);
                if verdict.raise_status {
                    set_status(SYS_NO_WIFI);
                }
                verdict.pause
            }
        };

        Timer::after(pause.try_into().expect("Wi-Fi retry pauses are seconds")).await;
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await;
}

/// Предел одной отправки: `send_to` ждёт места в tx-буфере, а без сети оно
/// может не освободиться.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Сон без событий не дольше этого: heartbeat watchdog и подхват показаний
/// из очереди (в живом режиме показание уходит не позже чем через столько).
const MAX_IDLE: Duration = Duration::from_secs(5);

/// Приём — только подтверждения (`ACK_DATAGRAM_LEN` = 39 Б).
const RX_BUF: usize = 256;
/// Передача — одна датаграмма `MAX_DATAGRAM` с запасом.
const TX_BUF: usize = 1280;
/// Записей о датаграммах в каждом буфере.
const PACKETS: usize = 4;

/// Бэклог отправителя (~80 КБ) собран при компиляции: создание в рантайме
/// провело бы его через стек.
static BACKLOG: ConstStaticCell<Backlog> = ConstStaticCell::new(Backlog::new());

/// Драйвер `meteo_core::sender::Sender` на UDP-сокете: кормит автомат
/// показаниями и подтверждениями, отправляет то, что он отдаёт, спит до его
/// дедлайна. Логика доставки (окно подтверждений, переотправка, живой режим)
/// — в автомате, с DST на хосте.
#[embassy_executor::task]
pub async fn network_send_loop(stack: Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; PACKETS];
    let mut rx_buffer = [0; RX_BUF];
    let mut tx_meta = [PacketMetadata::EMPTY; PACKETS];
    let mut tx_buffer = [0; TX_BUF];
    let mut incoming = [0u8; RX_BUF];

    let health = health();
    health.set(Word::NetStage, NetStage::WaitLink.code());
    stack.wait_link_up().await;
    stack.wait_config_up().await;

    let server: IpEndpoint = (SERVER_IP, SERVER_PORT).into();
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    // Порт 0 ⇒ embassy-net выдаёт динамический; сервер отвечает на него же.
    // Свежий сокет с неуказанным адресом привязывается всегда.
    socket
        .bind(0)
        .expect("a fresh UDP socket binds to an ephemeral port");

    // Радио включено с момента подключения ⇒ RNG истинно случайный (ESP32-C3
    // TRM, Random Number Generator). Две загрузки совпадут по BootId (и тогда
    // повторят nonce) с вероятностью 2⁻³² на пару — принятый риск протокола v2.
    let boot = BootId(Rng::new().random());
    println!("net: boot id {:08x}", boot.0);
    // Ключ постоянный ⇒ key schedule AES разворачиваем один раз на таску.
    // `new` от массива фиксированной длины упасть не может.
    let mut sender = Sender::new(Aes128Gcm::new(&SECRET_KEY.into()), boot, uptime(), BACKLOG.take());
    let mut mode = sender.mode();
    // Счётчик `net.ack_failures` растёт на переходе «отвечал → перестал»: сколько
    // раз сервер пропадал, а не сколько повторов было за один обрыв.
    let mut was_reachable = true;

    loop {
        // heartbeat для watchdog: итерация ограничена по времени (отправка —
        // SEND_TIMEOUT на датаграмму, ожидание — MAX_IDLE). Бьётся и при
        // лежащем сервере: повтор — не зависание.
        crate::watchdog::beat_net();

        health.set(Word::NetStage, NetStage::TakeReadings.code());
        take_readings(&mut sender, &health).await;
        // Снимок каждый оборот: тот же — отправитель его не заметит, новый
        // поедет ближайшей датаграммой.
        sender.on_health(health.snapshot());
        health.set(Word::NetStage, NetStage::Send.code());
        send_due(&socket, server, &mut sender).await;

        // Один писатель бита: эта таска, по слову автомата.
        let reachable = sender.server_reachable();
        if reachable {
            clear_status(SYS_NO_SERVER);
        } else {
            set_status(SYS_NO_SERVER);
            if was_reachable {
                health.bump(Word::AckFailures);
            }
        }
        was_reachable = reachable;

        let idle_end = Instant::now() + MAX_IDLE;
        let wake = sender.deadline().map_or(idle_end, |d| {
            u64::try_from(d.0.as_micros()).map_or(idle_end, |deadline_micros| {
                idle_end.min(Instant::from_micros(deadline_micros))
            })
        });
        health.set(Word::NetStage, NetStage::Wait.code());
        match with_deadline(wake, socket.recv_from(&mut incoming)).await {
            Ok(Ok((len, meta))) if meta.endpoint == server => {
                if let Err(e) = sender.on_datagram(&mut incoming[..len], uptime()) {
                    println!("net: server datagram rejected: {:?}", e);
                }
            }
            Ok(Ok((_, meta))) => println!("net: datagram from a stranger {:?} ignored", meta.endpoint),
            Ok(Err(e)) => println!("net: receive error: {:?}", e),
            Err(TimeoutError) => {}
        }

        if sender.mode() != mode {
            mode = sender.mode();
            match mode {
                Mode::Live { until } => println!("net: live mode until uptime {}s", until.0.as_secs()),
                Mode::Batch => println!("net: batch mode"),
            }
        }
    }
}

/// Забрать показания из передаточной очереди в бэклог. `SYS_BUF_OVERFLOW` —
/// один писатель, эта таска: потеря в очереди (счётчик sensor-таски) или
/// вытеснение из бэклога ставят бит, забор без потерь его гасит.
async fn take_readings(sender: &mut Sender<'_>, health: &Health<'_>) {
    // Замок только на перекладку — не через await'ы сокета, иначе
    // sensor-таска ждала бы его на enqueue.
    let mut queue = SENSOR_QUE.lock().await;
    if queue.is_empty() {
        return;
    }
    let queue_losses = queue_losses_since_last_take();
    health.add(Word::QueueEvictions, queue_losses);
    let mut lost = queue_losses > 0;
    while let Some(reading) = queue.dequeue() {
        if let Some(evicted) = sender.on_reading(reading) {
            println!("net: backlog full, dropped reading at {}", evicted.time.0);
            health.bump(Word::BacklogEvictions);
            lost = true;
        }
    }
    if lost {
        set_status(SYS_BUF_OVERFLOW);
    } else {
        clear_status(SYS_BUF_OVERFLOW);
    }
}

/// Сколько показаний вытеснено из передаточной очереди с прошлого забора.
fn queue_losses_since_last_take() -> u32 {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let now = QUEUE_EVICTIONS.load(Ordering::Relaxed);
    now.wrapping_sub(SEEN.swap(now, Ordering::Relaxed))
}

/// Отправить всё, что автомат считает пора отправить.
async fn send_due(socket: &UdpSocket<'_>, server: IpEndpoint, sender: &mut Sender<'_>) {
    while let Some(datagram) = sender.poll(uptime()) {
        let len = datagram.len();
        match with_timeout(SEND_TIMEOUT, socket.send_to(datagram, server)).await {
            Ok(Ok(())) => println!("net: sent {} B, backlog {}", len, sender.backlog_len()),
            Ok(Err(e)) => {
                println!("net: send error: {:?}", e);
                sender.on_send_failed(uptime());
                return;
            }
            Err(TimeoutError) => {
                println!("net: send timeout ({}s)", SEND_TIMEOUT.as_secs());
                sender.on_send_failed(uptime());
                return;
            }
        }
    }
}
