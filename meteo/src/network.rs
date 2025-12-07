use embassy_net::{Runner, Stack, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{
    WifiController, WifiDevice, WifiEvent, WifiStationState, scan::ScanConfig, sta::StationConfig,
};

use heapless::Vec;

use crate::sensor::{SENSOR_QUE, SensorData};

use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};

use postcard;

use esp_println::println;

// not the real crypto, because of reuse nonce!

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_radio::wifi::station_state() {
            WifiStationState::Connected => {
                // wait until we're no longer connected
                controller
                    .wait_for_event(WifiEvent::StationDisconnected)
                    .await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = esp_radio::wifi::ModeConfig::Station(
                StationConfig::default()
                    .with_ssid(WIFI_SSID.into())
                    .with_password(WIFI_PASSWD.into()),
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            println!("Scan");
            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                println!("{:?}", ap);
            }
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

pub async fn get_sensor_data_chunk() -> heapless::Vec<SensorData, 40> {
    let mut p = SENSOR_QUE.try_lock().ok();

    let mut out = heapless::Vec::<_, 40>::new();

    let Some(mut p) = p else { return out };

    for _ in 0..40 {
        if let Some(v) = p.dequeue() {
            out.push(v).unwrap();
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
        // Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(30)));

        println!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            Timer::after(Duration::from_millis(5000)).await;
            continue;
        }

        println!("connected!");
        // let mut buf = [0; 1024];

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
            let r = write_packet(&mut socket, p, noonce).await;

            // если есть прокси - пишет что всё ок но это не так. нужно явно сообщаться что не вышло
            match r {
                Ok(g) => {
                    println!("write! {}", g);
                    measurements_buf.clear();
                }
                Err(e) => {
                    println!("write error: {:?}", e);
                    Timer::after(Duration::from_millis(3000)).await;
                    break;
                }
            }

            // let n = match socket.read(&mut buf).await {
            //     Ok(0) => {
            //         println!("read EOF");
            //         break;
            //     }

            //     Ok(n) => n,
            //     Err(e) => {
            //         println!("read error: {:?}", e);
            //         break;
            //     }
            // };
            // println!("{}", core::str::from_utf8(&buf[..n]).unwrap());

            Timer::after(Duration::from_millis(3000)).await;
        }

        Timer::after(Duration::from_millis(3000)).await;
    }
}

#[allow(dead_code)]
async fn write_packet_simple(
    socket: &mut TcpSocket<'_>,
    p: heapless::Vec<SensorData, 40>,
) -> Result<usize, embassy_net::tcp::Error> {
    let mut body_buf = [0; 1024];

    println!("{:?}", &p);
    let payload_len = {
        let payload_buf = &mut body_buf[4..];
        let data_serialized = postcard::to_slice(p.as_slice(), payload_buf).unwrap();
        data_serialized.len()
    };

    let len_bytes = (payload_len as u32).to_be_bytes();
    body_buf[..4].copy_from_slice(&len_bytes);

    // реально полезная длина: 4 байта длины + payload_len
    let total_len = 4 + payload_len;

    // пишем только полезную часть
    let to_write = &body_buf[..total_len];

    let r = socket.write(&to_write).await;
    r
}

pub async fn write_packet(
    socket: &mut TcpSocket<'_>,
    p: &Vec<SensorData, 40>,
    nonce_counter: u64, // например, счётчик кадров
) -> Result<usize, embassy_net::tcp::Error> {
    let mut body_buf = [0u8; 1024];

    // сериализация
    let payload = {
        let payload_buf = &mut body_buf[..];
        postcard::to_slice(p.as_slice(), payload_buf).unwrap()
    };

    // AES-GCM
    let cipher = Aes128Gcm::new_from_slice(&SECRET_KEY).unwrap();

    // nonce = 96 бит (12 байт). Берём счётчик и паддим.
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[4..].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from_slice(&nonce_bytes);

    // зашифровать (шифр + тег аутентичности в конце)
    let ciphertext = cipher.encrypt(nonce, &*payload).unwrap();

    // теперь пакуем как обычно: [4 байта длины] + ciphertext
    let payload_len = ciphertext.len();
    let len_bytes = (payload_len as u32).to_be_bytes();

    body_buf[..4].copy_from_slice(&len_bytes);
    body_buf[4..4 + payload_len].copy_from_slice(&ciphertext);

    let total_len = 4 + payload_len;
    let to_write = &body_buf[..total_len];

    socket.write(to_write).await
}
