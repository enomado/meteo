use std::net::Ipv4Addr;

use toml::Value;

pub struct FirmwareConfig {
    /// Сети в порядке из конфига: [0] — основная, [1] — опциональная wifi2.
    /// Прошивка выбирает из них по силе сигнала (скан), см. `network::connection`.
    pub wifi_networks: Vec<FirmwareWifiNetwork>,
    pub server_ip:     Ipv4Addr,
    pub server_port:   u16,
    pub secret_key:    [u8; 16],
}

pub struct FirmwareWifiNetwork {
    pub ssid:     String,
    pub password: String,
}

/// Пределы esp-radio: `Ssid` держит 32 байта, `Password` — 64. Проверяем на
/// сборке, чтобы прошивка не падала на `try_into()` уже на устройстве: паника на
/// буте = бесконечный цикл reset'ов (см. `crate::watchdog` в прошивке).
const SSID_MAX_BYTES: usize = 32;
const PASSWD_MAX_BYTES: usize = 64;

fn check_network(ssid: &str, passwd: &str) {
    assert!(
        ssid.len() <= SSID_MAX_BYTES,
        "SSID {ssid:?} is {} bytes, esp-radio allows {SSID_MAX_BYTES}",
        ssid.len()
    );
    assert!(
        passwd.len() <= PASSWD_MAX_BYTES,
        "password for SSID {ssid:?} is {} bytes, esp-radio allows {PASSWD_MAX_BYTES}",
        passwd.len()
    );
}

pub fn parse_config(toml_str: &str) -> FirmwareConfig {
    let doc: Value = toml::from_str(toml_str).expect("invalid TOML");
    let fw = &doc["firmware"];

    let wifi_ssid = fw["wifi_ssid"].as_str().expect("no wifi_ssid").to_string();
    let wifi_passwd = fw["wifi_passwd"].as_str().expect("no wifi_passwd").to_string();

    let mut wifi_networks = vec![FirmwareWifiNetwork {
        ssid:     wifi_ssid,
        password: wifi_passwd,
    }];

    // wifi2 — опциональная вторая сеть. Обе половины (ssid+passwd) либо есть,
    // либо нет: полконфига — это молчаливо неработающая сеть, поэтому паникуем.
    match (fw.get("wifi2_ssid"), fw.get("wifi2_passwd")) {
        (Some(ssid), Some(passwd)) => {
            let ssid = ssid.as_str().expect("wifi2_ssid must be a string");
            let passwd = passwd.as_str().expect("wifi2_passwd must be a string");
            wifi_networks.push(FirmwareWifiNetwork {
                ssid:     ssid.to_string(),
                password: passwd.to_string(),
            });
        }
        (None, None) => {}
        _ => panic!("wifi2_ssid and wifi2_passwd must be set together"),
    }

    let server_ip_str = fw["server_ip"].as_str().expect("no server_ip");
    // let server_ip: IpAddr = server_ip_str.parse().expect("invalid IP");
    let server_ip: Ipv4Addr = server_ip_str.parse().expect("no server_ip");

    let server_port = u16::try_from(fw["server_port"].as_integer().expect("no server_port"))
        .expect("server_port must fit in u16");

    let secret_key_hex = fw["secret_key"].as_str().expect("no secret_key");
    let secret_key = hex_to_u8_16(secret_key_hex);

    for network in &wifi_networks {
        check_network(&network.ssid, &network.password);
    }

    FirmwareConfig {
        wifi_networks,
        server_ip,
        server_port,
        secret_key,
    }
}

pub fn secret_literal(secret: &[u8; 16]) -> String {
    secret.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")
}

pub fn wifi_network_literal(network: &FirmwareWifiNetwork) -> String {
    format!(
        "meteo_core::wifi_pick::Network {{ ssid: {:?}, password: {:?} }}",
        network.ssid, network.password
    )
}

pub fn hex_to_u8_16(hex: &str) -> [u8; 16] {
    assert_eq!(hex.len(), 32, "SECRET_KEY HEX must be 32 chars = 16 bytes");

    let mut bytes = [0u8; 16];
    for i in 0..16 {
        let byte_str = &hex[i * 2..i * 2 + 2];
        bytes[i] = u8::from_str_radix(byte_str, 16).expect("invalid hex");
    }
    bytes
}

pub fn ipv4_literal(ip: Ipv4Addr) -> String {
    let octets = ip.octets();
    format!(
        "core::net::Ipv4Addr::new({}, {}, {}, {})",
        octets[0], octets[1], octets[2], octets[3]
    )
}

#[cfg(test)]
mod tests {
    use super::{
        FirmwareWifiNetwork,
        wifi_network_literal,
    };

    #[test]
    fn wifi_network_literal_escapes_rust_string_special_characters() {
        let network = FirmwareWifiNetwork {
            ssid:     "quoted\"\\ssid".to_string(),
            password: "line\nbreak".to_string(),
        };

        assert_eq!(
            wifi_network_literal(&network),
            "meteo_core::wifi_pick::Network { ssid: \"quoted\\\"\\\\ssid\", password: \"line\\nbreak\" }"
        );
    }
}
