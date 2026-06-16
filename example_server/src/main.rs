//! Minimal reference receiver for the `meteo` firmware.
//!
//! The firmware (see `../meteo`) opens a TCP connection and streams encrypted
//! measurement packets. This example accepts those connections, decrypts and
//! decodes each packet, and prints the readings. It deliberately does *nothing*
//! else — wire up your own storage/backend where marked below.
//!
//! ## Wire protocol
//!
//! Each packet on the TCP stream is:
//!
//! ```text
//! [ u32 big-endian payload_len ][ AES-128-GCM ciphertext || 16-byte tag ]
//! ```
//!
//! - `payload_len` = ciphertext length + 16 (the GCM tag is appended inline).
//! - Cipher: AES-128-GCM, 16-byte shared key (must match the firmware's
//!   `config.toml` `secret_key`), empty associated data.
//! - Nonce (96 bit): 4 zero bytes followed by an 8-byte big-endian packet
//!   counter. The counter starts at 1 for the first packet of a connection and
//!   increments by one per packet. The firmware resets it on every reconnect,
//!   so the receiver simply counts per accepted connection.
//! - Plaintext: a postcard-serialized `Vec<SensorData>` (see the structs
//!   below — field order MUST match the firmware, postcard is order-based).
//!
//! Note: reusing a counter-based nonce across reboots is a known, intentional
//! weakness of this toy protocol — do not treat AES-GCM here as authenticated
//! transport security.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use anyhow::Context;
use chrono::{Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// 16-byte AES-128 key shared with the firmware. This is the demo value
/// (`config.toml` `secret_key = "73757065..."` == ASCII "supersecretkey!1").
/// Replace it with your own key and keep both sides in sync.
static KEY: [u8; 16] = *b"supersecretkey!1";

const DEFAULT_LISTEN: &str = "0.0.0.0:1234";
/// Hard cap so a malformed length prefix can't make us allocate wildly.
const MAX_PACKET: usize = 4 * 1024;

/// BMP390 reading — pressure (Pa) + temperature (°C). Both fields are filled or
/// absent together (read in one call on the firmware).
#[derive(Debug, Serialize, Deserialize)]
struct BaroReading {
    pressure: f32,
    temp: f32,
}

/// SCD41 reading — CO2 (ppm), humidity (%), temperature (°C).
#[derive(Debug, Serialize, Deserialize)]
struct ScdReading {
    co2: u16,
    humidity: f32,
    temp: f32,
}

/// One measurement. `baro`/`scd` are independent (a sensor may be missing or
/// produce nothing this cycle), so the row is sparse. `time` is milliseconds
/// since the Unix epoch (the firmware timestamps via NTP).
///
/// Field order must match the firmware's `SensorData` — postcard serializes by
/// declaration order, without names.
#[derive(Debug, Serialize, Deserialize)]
struct SensorData {
    baro: Option<BaroReading>,
    scd: Option<ScdReading>,
    time: u64,
}

/// Read one length-prefixed, AES-GCM-encrypted, postcard-encoded packet.
async fn read_packet(socket: &mut TcpStream, nonce_counter: u64) -> anyhow::Result<Vec<SensorData>> {
    // 1) length prefix
    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf).await?;
    let payload_len = u32::from_be_bytes(len_buf) as usize;
    if payload_len > MAX_PACKET {
        return Err(anyhow::anyhow!("packet too big: {payload_len} bytes"));
    }

    // 2) ciphertext || tag
    let mut cipher_buf = vec![0u8; payload_len];
    socket.read_exact(&mut cipher_buf).await?;

    // 3) decrypt: nonce = 4 zero bytes + 8-byte BE counter (mirrors firmware)
    let cipher = Aes128Gcm::new_from_slice(&KEY).expect("16-byte key");
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[4..].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, cipher_buf.as_ref())
        .ok()
        .context("AES-GCM decrypt failed (wrong key or out-of-sync nonce?)")?;

    // 4) decode
    let data: Vec<SensorData> = postcard::from_bytes(&plaintext).context("postcard decode failed")?;
    Ok(data)
}

/// Pretty-print a packet. This is the spot to replace with real persistence.
fn print_readings(addr: &std::net::SocketAddr, packet: &[SensorData]) {
    for s in packet {
        let when = Utc
            .timestamp_millis_opt(s.time as i64)
            .single()
            .map(|dt| dt.with_timezone(&Local).to_string())
            .unwrap_or_else(|| format!("{}ms", s.time));
        if let Some(b) = &s.baro {
            println!("[{addr}] {when}  P={:.1} hPa  T={:.2} °C", b.pressure / 100.0, b.temp);
        }
        if let Some(sc) = &s.scd {
            println!("[{addr}] {when}  CO2={} ppm  H={:.1}%  T={:.2} °C", sc.co2, sc.humidity, sc.temp);
        }
    }
}

async fn handle_client(mut stream: TcpStream, addr: std::net::SocketAddr) {
    println!("[{addr}] connected");
    let mut nonce_counter = 0u64;

    loop {
        nonce_counter += 1; // firmware's per-connection counter starts at 1
        let packet = match read_packet(&mut stream, nonce_counter).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[{addr}] disconnected: {e:?}");
                break;
            }
        };

        println!("[{addr}] packet #{nonce_counter}: {} reading(s)", packet.len());
        print_readings(&addr, &packet);

        // >>> Plug your backend here: write `packet` to a DB, a queue, a file,
        //     forward it over gRPC, etc. The firmware doesn't care what you do.
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen = std::env::var("METEO_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let listener = TcpListener::bind(&listen).await?;
    println!("meteo example receiver listening on {listen}");

    loop {
        let (socket, addr) = listener.accept().await?;
        tokio::spawn(handle_client(socket, addr));
    }
}
