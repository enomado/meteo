use core::net::Ipv4Addr;

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use embassy_net::tcp::{
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
use esp_println::println;
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig,
    Config,
    Interface,
    WifiController,
};
use meteo_core::sender::{
    Action,
    Event,
    Sender,
};
use meteo_core::wifi_pick::{
    Network,
    Rssi,
    round_robin,
    strongest,
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

/// socket-timeout: соединение без входящих сегментов дольше — мёртвое. Он же
/// ограничивает `connect` к недоступному серверу.
const TCP_TIMEOUT: Duration = Duration::from_secs(120);

/// Предел на одну операцию отправки: запись порции пакета или ожидание ACK.
/// Без него ожидание ограничено только socket-timeout'ом (120с, сбрасывается
/// любым входящим сегментом) и могло перерасти `NET_STALL_LIMIT` (180с) ⇒
/// ресет всего чипа вместо реконнекта.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Сколько ждём ухода RST после `abort`, прежде чем переиспользовать сокет.
/// Без сети RST не уйдёт никогда — поэтому предел; `connect` всё равно
/// начинает с чистого сокета.
const ABORT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Драйвер `meteo_core::sender::Sender`: исполняет его действия на TCP-сокете
/// и возвращает итог. Логика доставки (дописывание пакета, чистка батча только
/// после ACK, повтор после обрыва) — в автомате, с DST-тестом на хосте.
#[embassy_executor::task]
pub async fn network_send_loop(stack: Stack<'static>) {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 2048];

    stack.wait_link_up().await;
    stack.wait_config_up().await;

    let remote_endpoint = (SERVER_IP, SERVER_PORT);

    // Один сокет на всю жизнь таски: после обрыва он `abort`-ится и снова
    // уходит в `connect` (тот сбрасывает состояние, таймаут сохраняется).
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(TCP_TIMEOUT));

    // Ключ постоянный ⇒ key schedule AES разворачиваем один раз на таску, а не
    // на каждый пакет. `new` от массива фиксированной длины упасть не может.
    let mut sender = Sender::new(Aes128Gcm::new(&SECRET_KEY.into()));
    let mut outcome = perform(&mut socket, remote_endpoint, sender.start()).await;

    loop {
        // heartbeat для watchdog: одно действие на итерацию, каждое ограничено
        // по времени (connect — TCP_TIMEOUT, запись/ACK — SEND_TIMEOUT, сон ≤5с).
        // Бьётся и при лежащем сервере: retry — не зависание.
        crate::watchdog::beat_net();

        if outcome == Event::Flushed {
            println!("delivered {} readings", sender.in_flight());
        }
        let action = {
            // Замок только на время шага автомата — не через await'ы сокета,
            // иначе sensor-таска ждала бы его на enqueue.
            let mut queue = SENSOR_QUE.lock().await;
            sender.step(outcome, &mut queue)
        };
        outcome = perform(&mut socket, remote_endpoint, action).await;
    }
}

/// Исполнить действие автомата и вернуть его итог.
async fn perform(socket: &mut TcpSocket<'_>, endpoint: (Ipv4Addr, u16), action: Action<'_>) -> Event {
    match action {
        Action::Connect => {
            println!("connecting...");
            match socket.connect(endpoint).await {
                Ok(()) => {
                    println!("connected!");
                    clear_status(SYS_NO_TCP);
                    Event::Connected
                }
                Err(e) => {
                    println!("connect error: {:?}", e);
                    drop_connection(socket).await
                }
            }
        }
        // `write` берёт сколько влезло в tx-буфер; остаток автомат пришлёт
        // следующим `Write`.
        Action::Write(bytes) => {
            match with_timeout(SEND_TIMEOUT, socket.write(bytes)).await {
                Ok(Ok(n)) => Event::Written(n),
                Ok(Err(e)) => {
                    println!("write error: {:?}", e);
                    drop_connection(socket).await
                }
                Err(TimeoutError) => {
                    println!("write timeout ({}s)", SEND_TIMEOUT.as_secs());
                    drop_connection(socket).await
                }
            }
        }
        Action::Flush => {
            match with_timeout(SEND_TIMEOUT, socket.flush()).await {
                // `flush` embassy-net 0.9 отдаёт `Ok` и когда сокет ушёл в
                // `Closed` (RST/таймаут) с недоставленными данными: его условие
                // ожидания — `send_queue() > 0 && state() != Closed`. Доставку
                // подтверждает только живое состояние после него.
                Ok(Ok(())) if socket.state() != State::Closed => Event::Flushed,
                Ok(Ok(())) => {
                    println!("connection closed before ACK");
                    drop_connection(socket).await
                }
                Ok(Err(e)) => {
                    println!("flush error: {:?}", e);
                    drop_connection(socket).await
                }
                Err(TimeoutError) => {
                    println!("send timeout: no ACK in {}s", SEND_TIMEOUT.as_secs());
                    drop_connection(socket).await
                }
            }
        }
        Action::Sleep(pause) => {
            Timer::after(Duration::from_millis(pause.as_millis() as u64)).await;
            Event::Woke
        }
    }
}

/// Закрыть соединение после ошибки: сокет готов к новому `connect`.
async fn drop_connection(socket: &mut TcpSocket<'_>) -> Event {
    set_status(SYS_NO_TCP);
    socket.abort();
    if with_timeout(ABORT_FLUSH_TIMEOUT, socket.flush()).await.is_err() {
        println!(
            "RST not sent in {}s, reusing socket anyway",
            ABORT_FLUSH_TIMEOUT.as_secs()
        );
    }
    Event::Failed
}
