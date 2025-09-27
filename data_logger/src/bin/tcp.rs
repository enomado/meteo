#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, Nonce};

impl SensorData {
    fn convert(&self) -> SensorDataClean {
        let time = Utc.timestamp_millis_opt(self.time as i64).unwrap();
        SensorDataClean {
            pressure: self.pressure,
            temp: self.temp,
            time: time,
        }
    }
}

// fn from_epoch_millis(ms: i64) -> DateTime<Utc> {
//     let secs = ms / 1000;
//     let nsecs = ((ms % 1000) * 1_000_000) as u32;
//     Utc.timestamp_millis_opt(secs, nsecs).unwrap()
// }

#[derive(Debug, Clone)]
pub struct SensorDataClean {
    pub pressure: f32,
    pub temp: f32,
    // millis epoch
    pub time: DateTime<Utc>,
}

fn read_postcard_simple<T: for<'de> serde::Deserialize<'de>>(
    socket: &mut TcpStream,
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf)?;
    let payload_len = u32::from_be_bytes(len_buf) as usize;

    let mut payload_buf = vec![0u8; payload_len];
    socket.read_exact(&mut payload_buf)?;

    let obj = postcard::from_bytes(&payload_buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    Ok(obj)
}

static KEY: [u8; 16] = *b"supersecretkey!1"; // 128-бит ключ

use aes_gcm::KeyInit;
use anyhow::Context;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use itertools::Itertools;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SensorData {
    pub pressure: f32,
    pub temp: f32,
    // millis epoch
    pub time: u64,
}

pub fn read_packet<'a>(
    // socket: &mut TcpSocket<'_>,
    socket: &mut TcpStream,
    nonce_counter: u64,
) -> anyhow::Result<Vec<SensorData>> {
    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf).unwrap();
    let payload_len = u32::from_be_bytes(len_buf) as usize;

    // читаем ciphertext
    let mut cipher_buf = [0u8; 1024];
    let ciphertext = &mut cipher_buf[..payload_len];
    socket.read_exact(ciphertext)?;

    // AES-GCM дешифрование
    let cipher = Aes128Gcm::new_from_slice(&KEY).unwrap();

    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[4..].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .ok()
        .context("cant decode")?;

    // десериализация postcard
    let data: Vec<SensorData> = postcard::from_bytes(&plaintext).unwrap();

    Ok(data)
}

fn handle_client(mut stream: TcpStream) {
    let mut buf = [0u8; 8024];

    let mut nonce_counter = 0;

    loop {
        nonce_counter += 1;
        // let s = read_postcard_simple::<Vec<SensorData>>(&mut stream).unwrap();

        let s = read_packet(&mut stream, nonce_counter).unwrap();

        let s = s.iter().map(|s| s.convert()).collect_vec();

        for o in s {
            let pressure_mm = o.pressure / 133.322;
            let pressure_hpa = o.pressure / 100.0;

            let local_time = o.time.with_timezone(&Local);

            println!(
                "{}, {:0.4}, {:0.4}, {:0.4}",
                local_time, pressure_mm, pressure_hpa, o.temp
            );
        }

        // match stream.read(&mut buf) {
        //     Ok(0) => break, // клиент отключился
        //     Ok(n) => {
        //         let data = &buf[..n];

        //         let s = postcard::from_bytes::<SensorData>(data).unwrap();

        //         dbg!(n, s);
        //     }
        //     Err(e) => {
        //         eprintln!("Ошибка чтения: {}", e);
        //         break;
        //     }
        // }
    }
}

fn main() -> std::io::Result<()> {
    //

    unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    println!("starting");
    let addr = "0.0.0.0:1234";
    let listener = TcpListener::bind(addr)?;
    println!("works");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(|| handle_client(stream));
            }
            Err(e) => eprintln!("Ошибка подключения: {}", e),
        }
    }

    Ok(())
}
