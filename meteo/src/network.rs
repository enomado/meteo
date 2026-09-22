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
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig,
    Config,
    Interface,
    WifiController,
};
use meteo_core::datagram::BootId;
use meteo_core::sender::{
    Backlog,
    Mode,
    Sender,
};
use meteo_core::wifi_pick::{
    Network,
    Rssi,
    round_robin,
    strongest,
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
use crate::watchdog::uptime;

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

/// Сколько AP забираем из скана. Скан отдаёт список, отсортированный по RSSI;
/// 20 с запасом покрывает и людное окружение — нам нужны лишь свои SSID.
const SCAN_MAX_APS: usize = 20;

/// Выбирает сеть из `WIFI_NETWORKS` с самым сильным сигналом (решение —
/// `meteo_core::wifi_pick`, здесь только скан).
///
/// Одна сеть в конфиге — выбирать не из чего, скан пропускаем (это ~2с радио на
/// каждый реконнект). Если скан упал или ни одного своего SSID в эфире нет —
/// перебираем сети по кругу (`attempt`).
async fn pick_network(controller: &mut WifiController<'static>, attempt: usize) -> Network<'static> {
    let fallback = *round_robin(WIFI_NETWORKS, attempt);

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

    let seen = aps.iter().map(|ap| (ap.ssid.as_str(), Rssi(ap.signal_strength)));
    match strongest(WIFI_NETWORKS, seen) {
        Some((net, rssi)) => {
            println!("scan: picked {} (rssi {})", net.0, rssi.0);
            *net
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

    loop {
        // heartbeat для watchdog: итерация ограничена по времени (отправка —
        // SEND_TIMEOUT на датаграмму, ожидание — MAX_IDLE). Бьётся и при
        // лежащем сервере: повтор — не зависание.
        crate::watchdog::beat_net();

        take_readings(&mut sender).await;
        send_due(&socket, server, &mut sender).await;

        // Один писатель бита: эта таска, по слову автомата.
        if sender.server_reachable() {
            clear_status(SYS_NO_SERVER);
        } else {
            set_status(SYS_NO_SERVER);
        }

        let idle_end = Instant::now() + MAX_IDLE;
        let wake = sender.deadline().map_or(idle_end, |d| {
            idle_end.min(Instant::from_micros(d.0.as_micros() as u64))
        });
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
async fn take_readings(sender: &mut Sender<'_>) {
    // Замок только на перекладку — не через await'ы сокета, иначе
    // sensor-таска ждала бы его на enqueue.
    let mut queue = SENSOR_QUE.lock().await;
    if queue.is_empty() {
        return;
    }
    let mut lost = queue_losses_since_last_take();
    while let Some(reading) = queue.dequeue() {
        if let Some(evicted) = sender.on_reading(reading) {
            println!("net: backlog full, dropped reading at {}", evicted.time.0);
            lost = true;
        }
    }
    if lost {
        set_status(SYS_BUF_OVERFLOW);
    } else {
        clear_status(SYS_BUF_OVERFLOW);
    }
}

/// Были ли потери в передаточной очереди с прошлого забора.
fn queue_losses_since_last_take() -> bool {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let now = QUEUE_EVICTIONS.load(Ordering::Relaxed);
    SEEN.swap(now, Ordering::Relaxed) != now
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
