#[allow(unused_imports)]
use std::{
    env, fs,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
};

#[allow(dead_code)]
/// Преобразует IpAddr в Rust-литерал
pub fn ip_literal(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!(
                "core::net::IpAddr::V4(core::net::Ipv4Addr::new({}, {}, {}, {}))",
                o[0], o[1], o[2], o[3]
            )
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!(
                "core::net::IpAddr::V6(core::net::Ipv6Addr::new({}, {}, {}, {}, {}, {}, {}, {}))",
                s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]
            )
        }
    }
}

pub struct FirmwareConfig {
    pub wifi_ssid: String,
    pub wifi_passwd: String,
    pub server_ip: Ipv4Addr,
    pub server_port: u16,
    pub secret_key: [u8; 16],
}

use toml::Value;

pub fn parse_config(toml_str: &str) -> FirmwareConfig {
    let doc: Value = toml::from_str(toml_str).expect("invalid TOML");
    let fw = &doc["firmware"];

    let wifi_ssid = fw["wifi_ssid"].as_str().expect("no wifi_ssid").to_string();
    let wifi_passwd = fw["wifi_passwd"]
        .as_str()
        .expect("no wifi_passwd")
        .to_string();
    let server_ip_str = fw["server_ip"].as_str().expect("no server_ip");
    // let server_ip: IpAddr = server_ip_str.parse().expect("invalid IP");
    let server_ip: Ipv4Addr = server_ip_str.parse().expect("no server_ip");

    let server_port = fw["server_port"].as_integer().expect("no server_port") as u16;

    let secret_key_hex = fw["secret_key"].as_str().expect("no secret_key");
    let secret_key = hex_to_u8_16(secret_key_hex);

    FirmwareConfig {
        wifi_ssid,
        wifi_passwd,
        server_ip,
        secret_key,
        server_port,
    }
}

pub fn secret_literal(secret: &[u8; 16]) -> String {
    secret
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn hex_to_u8_16(hex: &str) -> [u8; 16] {
    if hex.len() != 32 {
        panic!("SECRET_KEY HEX must be 32 chars = 16 bytes");
    }

    let mut bytes = [0u8; 16];
    for i in 0..16 {
        let byte_str = &hex[i * 2..i * 2 + 2];
        bytes[i] = u8::from_str_radix(byte_str, 16).expect("invalid hex");
    }
    bytes
}

pub fn ipv4_literal(ip: std::net::Ipv4Addr) -> String {
    let octets = ip.octets();
    format!(
        "core::net::Ipv4Addr::new({}, {}, {}, {})",
        octets[0], octets[1], octets[2], octets[3]
    )
}
