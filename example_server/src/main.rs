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
//! - Plaintext: a postcard-serialized sequence of `SensorData`.
//!
//! The structs and the encode/decode code live in `../meteo_core` (`wire`
//! module) and are shared with the firmware, so both sides cannot drift apart.
//!
//! Note: the counter-based nonce repeats on every reconnect — a known weakness
//! of this toy protocol, do not treat AES-GCM here as authenticated transport
//! security (fix planned in `docs/PLAN_hardening.md`, stage 3).

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use anyhow::Context;
use chrono::{
    Local,
    TimeZone,
    Utc,
};
use meteo_core::wire::{
    LEN_PREFIX,
    PacketCounter,
    SensorBatch,
    SensorData,
    decode_payload,
    payload_len,
};
use tokio::io::AsyncReadExt;
use tokio::net::{
    TcpListener,
    TcpStream,
};

/// 16-byte AES-128 key shared with the firmware. This is the demo value
/// (`config.toml` `secret_key = "73757065..."` == ASCII "supersecretkey!1").
/// Replace it with your own key and keep both sides in sync.
static KEY: [u8; 16] = *b"supersecretkey!1";

const DEFAULT_LISTEN: &str = "0.0.0.0:1234";

/// Read one length-prefixed, AES-GCM-encrypted, postcard-encoded packet.
async fn read_packet(
    socket: &mut TcpStream,
    cipher: &Aes128Gcm,
    counter: PacketCounter,
) -> anyhow::Result<SensorBatch> {
    // 1) length prefix — range-checked before reading, so a malformed prefix
    //    can't make us allocate wildly
    let mut len_buf = [0u8; LEN_PREFIX];
    socket.read_exact(&mut len_buf).await?;
    let payload_len = payload_len(len_buf).map_err(|e| anyhow::anyhow!("bad frame: {e:?}"))?;

    // 2) ciphertext || tag
    let mut payload = vec![0u8; payload_len];
    socket.read_exact(&mut payload).await?;

    // 3) decrypt + decode
    decode_payload(cipher, counter, &mut payload)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("packet rejected (wrong key or out-of-sync nonce?)")
}

/// Pretty-print a packet. This is the spot to replace with real persistence.
fn print_readings(addr: &std::net::SocketAddr, packet: &[SensorData]) {
    for s in packet {
        let when = Utc
            .timestamp_millis_opt(s.time.0 as i64)
            .single()
            .map(|dt| dt.with_timezone(&Local).to_string())
            .unwrap_or_else(|| format!("{}ms", s.time.0));
        if let Some(b) = &s.baro {
            println!(
                "[{addr}] {when}  P={:.1} hPa  T={:.2} °C",
                b.pressure / 100.0,
                b.temp
            );
        }
        if let Some(sc) = &s.scd {
            println!(
                "[{addr}] {when}  CO2={} ppm  H={:.1}%  T={:.2} °C",
                sc.co2, sc.humidity, sc.temp
            );
        }
    }
}

async fn handle_client(mut stream: TcpStream, addr: std::net::SocketAddr) {
    println!("[{addr}] connected");
    let cipher = Aes128Gcm::new(&KEY.into());
    // firmware's per-connection counter starts at 1
    let mut counter = PacketCounter::FIRST;

    loop {
        let packet = match read_packet(&mut stream, &cipher, counter).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[{addr}] disconnected: {e:?}");
                break;
            }
        };

        println!("[{addr}] packet #{}: {} reading(s)", counter.0, packet.len());
        print_readings(&addr, &packet);
        counter = counter.next();

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
