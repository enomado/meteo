//! Minimal reference receiver for the `meteo` firmware.
//!
//! It accepts the firmware's data, decrypts and decodes it, prints the
//! readings and acknowledges them. It deliberately does *nothing* else — wire
//! up your own storage/backend where marked below.
//!
//! ## Protocol v2 (UDP, current firmware)
//!
//! One datagram, at most 1172 bytes:
//!
//! ```text
//! [ 9-byte header ][ AES-128-GCM ciphertext ][ 16-byte tag ]
//! ```
//!
//! - Header, big-endian, authenticated as associated data: `b0` (version in
//!   bits 7..4, direction in bit 3, ACK_REQ in bit 0), then boot id + sequence
//!   number (firmware → server) or server salt + ack number (server → firmware).
//!   The nonce is built from the header, so loss, reordering and duplicates do
//!   not matter to decryption.
//! - Plaintext: fixed-point readings, delta + varint encoded within one
//!   datagram (`meteo_core::codec`).
//! - Datagrams with ACK_REQ are answered with an acknowledgement *after* the
//!   readings are stored: a window of which sequence numbers of that boot are
//!   stored, plus how long the firmware should stay in live mode (send every
//!   reading immediately). Readings the window does not confirm are resent by
//!   the firmware in a new datagram, so storage must be idempotent by reading
//!   time.
//!
//! Receiver logic (verification, duplicate window, ack) lives in
//! `meteo_core::server` and is shared with the production receiver; this file
//! is only the socket loop.
//!
//! ## Protocol v1 (TCP, older firmware)
//!
//! `[ u32 BE payload_len ][ AES-128-GCM ciphertext || tag ]` per packet,
//! postcard-encoded `SensorData` batch, nonce from a per-connection counter
//! (repeats on every reconnect — not transport security). Kept until old
//! firmware is gone (`docs/PLAN_hardening.md`, stage 3.7 step 4).

use std::net::SocketAddr;
use std::time::Instant;

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
use meteo_core::codec::Reading;
use meteo_core::datagram::{
    LiveSecs,
    ServerSalt,
};
use meteo_core::server::Server;
use meteo_core::wire::{
    EpochMillis,
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
    UdpSocket,
};

/// 16-byte AES-128 key shared with the firmware. This is the demo value
/// (`config.toml` `secret_key = "73757065..."` == ASCII "supersecretkey!1").
/// Replace it with your own key and keep both sides in sync.
static KEY: [u8; 16] = *b"supersecretkey!1";

const DEFAULT_LISTEN: &str = "0.0.0.0:1234";

/// Receive buffer: larger than any valid datagram, so an oversized one is
/// rejected as such instead of being silently truncated.
const UDP_BUF: usize = 2048;

fn local_time(time: EpochMillis) -> String {
    Utc.timestamp_millis_opt(time.0 as i64)
        .single()
        .map(|dt| dt.with_timezone(&Local).to_string())
        .unwrap_or_else(|| format!("{}ms", time.0))
}

/// Pretty-print v2 readings. This is the spot to replace with real persistence.
fn print_readings(addr: &SocketAddr, readings: &[Reading]) {
    for r in readings {
        let mut line = format!("[{addr}] {}", local_time(r.time));
        if let Some(p) = r.pressure {
            line += &format!("  P={:.2} hPa", p.pascal() / 100.0);
        }
        if let Some(t) = r.baro_temp {
            line += &format!("  T_baro={:.3} °C", t.celsius());
        }
        if let Some(co2) = r.co2 {
            line += &format!("  CO2={} ppm", co2.0);
        }
        if let Some(h) = r.humidity {
            line += &format!("  H={:.3}%", h.percent());
        }
        if let Some(t) = r.scd_temp {
            line += &format!("  T_scd={:.3} °C", t.celsius());
        }
        println!("{line}");
    }
}

/// Protocol v2 loop: verify → store → mark → acknowledge, in this order
/// (`meteo_core::server` explains why).
async fn serve_udp(socket: UdpSocket, live_for: LiveSecs) -> anyhow::Result<()> {
    let salt = ServerSalt(getrandom::u32().map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?);
    let mut server = Server::new(Aes128Gcm::new(&KEY.into()), salt);
    let started = Instant::now();
    let mut buf = vec![0u8; UDP_BUF];

    loop {
        let (len, addr) = socket.recv_from(&mut buf).await.context("UDP receive")?;
        let now = started.elapsed();
        let incoming = match server.receive(&mut buf[..len], now) {
            Ok(incoming) => incoming,
            Err(e) => {
                eprintln!("[{addr}] datagram rejected: {e:?}");
                continue;
            }
        };
        let (boot, seq, ack_req) = (incoming.boot, incoming.seq, incoming.ack_req);

        if incoming.duplicate {
            println!(
                "[{addr}] boot {:08x} #{}: duplicate, already stored",
                boot.0, seq.0
            );
        } else {
            let readings = match incoming.collect_readings() {
                Ok(readings) => readings,
                Err(e) => {
                    eprintln!("[{addr}] boot {:08x} #{}: undecodable: {e:?}", boot.0, seq.0);
                    continue;
                }
            };
            println!(
                "[{addr}] boot {:08x} #{}: {} reading(s)",
                boot.0,
                seq.0,
                readings.len()
            );
            print_readings(&addr, &readings);

            // >>> Plug your backend here: store `readings` (idempotently by
            //     `time`). Call `ingested` ONLY after the store succeeded; on
            //     failure `continue` — no ack, the firmware resends.
            server.ingested(boot, seq, now);
        }

        if ack_req && let Some(ack) = server.ack(boot, live_for) {
            socket.send_to(&ack, addr).await.context("UDP send ack")?;
        }
    }
}

/// Read one length-prefixed, AES-GCM-encrypted, postcard-encoded v1 packet.
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

fn print_v1_readings(addr: &SocketAddr, packet: &[SensorData]) {
    for s in packet {
        let when = local_time(s.time);
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

async fn handle_tcp_client(mut stream: TcpStream, addr: SocketAddr) {
    println!("[{addr}] v1 TCP client connected");
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
        print_v1_readings(&addr, &packet);
        counter = counter.next();
    }
}

async fn serve_tcp(listener: TcpListener) -> anyhow::Result<()> {
    loop {
        let (socket, addr) = listener.accept().await?;
        tokio::spawn(handle_tcp_client(socket, addr));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen = std::env::var("METEO_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    // Seconds of live mode to grant in every ack (0 = batches only): lets you
    // try live mode without a frontend.
    let live_for = match std::env::var("METEO_LIVE_SECS") {
        Ok(v) => LiveSecs(v.parse().context("METEO_LIVE_SECS must be 0..=65535 seconds")?),
        Err(_) => LiveSecs::OFF,
    };

    let udp = UdpSocket::bind(&listen).await?;
    let tcp = TcpListener::bind(&listen).await?;
    println!(
        "meteo example receiver on {listen}: v2 over UDP (live_for {} s), v1 over TCP",
        live_for.0
    );

    tokio::select! {
        res = serve_udp(udp, live_for) => res,
        res = serve_tcp(tcp) => res,
    }
}
