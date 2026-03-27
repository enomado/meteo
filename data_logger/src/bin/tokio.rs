use aes_gcm::{Aes128Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, bail};
use chrono::{DateTime, Local, TimeZone, Utc};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_postgres::NoTls;

static KEY: [u8; 16] = *b"supersecretkey!1"; // 128-бит ключ
const ADDR: &str = "0.0.0.0:1234";

#[derive(Debug, Serialize, Deserialize)]
pub struct SensorData {
    pub pressure: Option<f32>,
    pub temp: Option<f32>,
    pub co2: Option<u16>,
    pub humidity: Option<f32>,
    // millis epoch
    pub time: u64,
}

impl SensorData {
    fn convert(&self) -> ConvertedSensor {
        let dt = Utc.timestamp_millis_opt(self.time as i64).unwrap();
        ConvertedSensor {
            pressure: self.pressure,
            temp: self.temp,
            co2: self.co2,
            humidity: self.humidity,
            time: dt,
        }
    }
}

#[derive(Debug)]
pub struct ConvertedSensor {
    pub pressure: Option<f32>,
    pub temp: Option<f32>,
    pub co2: Option<u16>,
    pub humidity: Option<f32>,
    pub time: DateTime<Utc>,
}

async fn read_packet(
    socket: &mut TcpStream,
    nonce_counter: u64,
) -> anyhow::Result<Vec<SensorData>> {
    // читаем длину
    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf).await?;
    let payload_len = u32::from_be_bytes(len_buf) as usize;

    if payload_len > 4 * 1024 {
        dbg!("packet is too big");
        return Err(anyhow::anyhow!("packet is too big"));
    }

    // читаем ciphertext
    let mut cipher_buf = vec![0u8; payload_len];
    socket.read_exact(&mut cipher_buf).await?;

    // AES-GCM дешифрование
    let cipher = Aes128Gcm::new_from_slice(&KEY).unwrap();

    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[4..].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, cipher_buf.as_ref())
        .ok()
        .context("cant decode")?;

    // десериализация postcard
    let data: Vec<SensorData> = postcard::from_bytes(&plaintext).unwrap();

    Ok(data)
}

async fn handle_client(mut stream: TcpStream) {
    let mut nonce_counter = 0;

    let (client, connection) =
        tokio_postgres::connect("host=/var/run/postgresql user=sc dbname=meteo", NoTls)
            .await
            .unwrap();

    // spawn — чтобы коннект в фоне жил
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    loop {
        nonce_counter += 1;

        let result: Result<Vec<SensorData>, anyhow::Error> =
            read_packet(&mut stream, nonce_counter).await;
        let s = match result {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Ошибка при чтении пакета: {:?}", e);
                break;
            }
        };

        let s = s.iter().map(|s| s.convert()).collect_vec();

        for o in s {
            let local_time = o.time.with_timezone(&Local);

            if let Some(pressure) = o.pressure {
                let pressure_mm = pressure / 133.322;
                let pressure_hpa = pressure / 100.0;
                println!(
                    "{}, P: {:0.4} mmHg, {:0.4} hPa, T: {:0.4}",
                    local_time, pressure_mm, pressure_hpa, o.temp.unwrap_or(0.0)
                );
            }

            if let Some(co2) = o.co2 {
                println!(
                    "{}, CO2: {} ppm, T: {:0.2}, H: {:0.2}%",
                    local_time, co2, o.temp.unwrap_or(0.0), o.humidity.unwrap_or(0.0)
                );
            }

            log_sensor_data(&client, &o).await.unwrap();
        }
    }
}

async fn log_sensor_data(
    client: &tokio_postgres::Client,
    data: &ConvertedSensor,
) -> anyhow::Result<u64> {
    let dt = data.time;
    let co2: Option<i32> = data.co2.map(|v| v as i32);

    let rows_affected = client
        .execute(
            "INSERT INTO sensor (time, pressure, temp, co2, humidity) VALUES ($1, $2, $3, $4, $5)",
            &[&dt, &data.pressure, &data.temp, &co2, &data.humidity],
        )
        .await?;

    Ok(rows_affected)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = TcpListener::bind(ADDR).await?;
    println!("Сервер слушает на {}", ADDR);

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("Новое соединение: {}", addr);

        tokio::spawn(async move {
            handle_client(socket).await;
        });
    }
}
