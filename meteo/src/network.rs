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

pub async fn get_sensor_data_chunk() -> heapless::Vec<SensorData, 40> {
    let mut out = heapless::Vec::<_, 40>::new();
    let Ok(mut p) = SENSOR_QUE.try_lock() else {
        return out;
    };

    while let Some(v) = p.dequeue() {
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
                Err(e) => {
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

/// On-wire layout: `[u32 BE payload_len][AES-GCM ciphertext][16-byte tag]`
/// где `payload_len` = ciphertext_len + 16 (tag inline).
/// Шифруем in-place в `body_buf[4..]`, tag дописываем сразу после — без heap-Vec.
pub async fn write_packet(
    socket: &mut TcpSocket<'_>,
    p: &Vec<SensorData, 40>,
    nonce_counter: u64,
) -> Result<usize, embassy_net::tcp::Error> {
    const TAG_LEN: usize = 16;
    const BUF_LEN: usize = 1024;
    let mut body_buf = [0u8; BUF_LEN];

    // postcard в body_buf[4..], оставив запас под tag в конце.
    let plain_len = postcard::to_slice(p.as_slice(), &mut body_buf[4..BUF_LEN - TAG_LEN])
        .unwrap()
        .len();

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
    socket.write(&body_buf[..total_len]).await
}
