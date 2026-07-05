use embassy_net::{Runner, Stack, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{Interface, WifiController};

use heapless::Vec;

use crate::led::{SYS_NO_TCP, SYS_NO_WIFI, clear_status, set_status};
use crate::sensor::{SENSOR_QUE, SensorData};

use aes_gcm::{AeadInPlace, Aes128Gcm, KeyInit, Nonce};

use postcard;

use esp_println::println;

// not the real crypto, because of reuse nonce!

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    loop {
        println!("About to connect...");

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

/// Максимум записей в один пакет. Бюджет postcard в write_packet = 1004 байта
/// (body_buf[4..1024-16]); одна SensorData ≤ ~32 байт (baro 9 + scd 13 + time 10
/// varint). 24 × 32 + 1 = 769 < 1004 — с большим запасом, overflow невозможен.
/// Остаток очереди (до 60) дренится следующими пакетами (каждые ~3с).
const MAX_BATCH: usize = 24;

pub async fn get_sensor_data_chunk() -> heapless::Vec<SensorData, 40> {
    let mut out = heapless::Vec::<_, 40>::new();
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

    let mut measurements_buf: heapless::Vec<SensorData, 40> = Vec::new();

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

        let mut noonce = 0u64;

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

            noonce += 1;
            println!("sending {} measurements, nonce={}", p.len(), noonce);
            let r = write_packet(&mut socket, p, noonce).await;

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
pub enum SendError {
    /// postcard не влез в фикс-буфер (батч слишком большой). При MAX_BATCH
    /// недостижимо; пришло на смену `.unwrap()`, который морозил чип.
    Serialize,
    /// Ошибка записи в сокет — рвём и реконнектимся.
    Tcp(embassy_net::tcp::Error),
}

/// On-wire layout: `[u32 BE payload_len][AES-GCM ciphertext][16-byte tag]`
/// где `payload_len` = ciphertext_len + 16 (tag inline).
/// Шифруем in-place в `body_buf[4..]`, tag дописываем сразу после — без heap-Vec.
pub async fn write_packet(
    socket: &mut TcpSocket<'_>,
    p: &Vec<SensorData, 40>,
    nonce_counter: u64,
) -> Result<usize, SendError> {
    const TAG_LEN: usize = 16;
    const BUF_LEN: usize = 1024;
    let mut body_buf = [0u8; BUF_LEN];

    // postcard в body_buf[4..], оставив запас под tag в конце. Overflow → Err
    // (НЕ паника): при MAX_BATCH недостижимо, но fail-safe важнее — паника здесь
    // морозила чип навсегда (инцидент 2026-07-04).
    let plain_len = match postcard::to_slice(p.as_slice(), &mut body_buf[4..BUF_LEN - TAG_LEN]) {
        Ok(s) => s.len(),
        Err(_) => return Err(SendError::Serialize),
    };

    let cipher = Aes128Gcm::new_from_slice(&SECRET_KEY).unwrap();

    // nonce = 96 бит: 4 байта паддинга + 8 байт BE-counter. ВНИМАНИЕ: reuse при
    // рестарте MCU (counter сбрасывается) — это известная (намеренная) дыра.
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[4..].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from_slice(&nonce_bytes);

    // шифрование in-place, tag отдельно
    let tag = cipher
        .encrypt_in_place_detached(nonce, b"", &mut body_buf[4..4 + plain_len])
        .unwrap();
    body_buf[4 + plain_len..4 + plain_len + TAG_LEN].copy_from_slice(&tag);

    let payload_len = plain_len + TAG_LEN;
    body_buf[..4].copy_from_slice(&(payload_len as u32).to_be_bytes());

    let total_len = 4 + payload_len;
    socket.write(&body_buf[..total_len]).await.map_err(SendError::Tcp)
}
