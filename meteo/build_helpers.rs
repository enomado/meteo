use std::fmt::Write as _;
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

/// Предел esp-radio: `Ssid` держит 32 байта. Проверяем на сборке, чтобы
/// прошивка не падала на `try_into()` уже на устройстве: паника на буте =
/// бесконечный цикл reset'ов (см. `crate::watchdog` в прошивке).
const SSID_MAX_BYTES: usize = 32;

/// WPA2 passphrase — 8..=63 печатных ASCII (IEEE 802.11i, H.4.1).
const PASSPHRASE_BYTES: core::ops::RangeInclusive<usize> = 8..=63;
/// PSK в hex — 64 символа; столько же держит `Password` esp-radio.
const PSK_HEX_LEN: usize = 64;
/// Итерации PBKDF2 по 802.11i.
const PSK_ROUNDS: u32 = 4096;

fn check_ssid(ssid: &str) {
    assert!(
        ssid.len() <= SSID_MAX_BYTES,
        "SSID {ssid:?} is {} bytes, esp-radio allows {SSID_MAX_BYTES}",
        ssid.len()
    );
}

/// WPA2 PSK в hex (64 символа) для пары SSID/passphrase.
///
/// Прошивке отдаём PSK, а не passphrase: получив 64 hex-символа, supplicant
/// блоба берёт их как PSK (`hexstr2bin` в `wpa.c`), иначе считает
/// PBKDF2-HMAC-SHA1 на 4096 итераций программным SHA-1 (`sha1-internal.c`) —
/// на ESP32-C3 это ~1.4 с занятого CPU на каждом подключении (плата,
/// 2026-10-18: шаг стартового теста LED 400 мс растягивался до 1.8 с).
/// Цена — WPA3-SAE с PSK невозможен; прошивка и так подключается как
/// `Wpa2Personal`.
///
/// Passphrase, уже записанная в конфиге как 64 hex-символа, — это PSK:
/// возвращаем её как есть, ровно как поступил бы supplicant.
fn wpa_psk_hex(ssid: &str, passphrase: &str) -> String {
    if passphrase.len() == PSK_HEX_LEN && passphrase.bytes().all(|b| b.is_ascii_hexdigit()) {
        return passphrase.to_ascii_lowercase();
    }
    assert!(
        PASSPHRASE_BYTES.contains(&passphrase.len()),
        "passphrase for SSID {ssid:?} is {} bytes; WPA2 needs 8..=63 (or a 64-hex PSK)",
        passphrase.len()
    );
    assert!(
        passphrase.bytes().all(|b| (0x20..=0x7e).contains(&b)),
        "passphrase for SSID {ssid:?} must be printable ASCII (IEEE 802.11i H.4.1)"
    );
    let psk: [u8; 32] =
        pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 32>(passphrase.as_bytes(), ssid.as_bytes(), PSK_ROUNDS);
    psk.iter().fold(String::with_capacity(PSK_HEX_LEN), |mut hex, b| {
        write!(hex, "{b:02x}").expect("writing to a String cannot fail");
        hex
    })
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
        check_ssid(&network.ssid);
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
        "meteo_core::wifi_pick::Network {{ ssid: {:?}, psk_hex: {:?} }}",
        network.ssid,
        wpa_psk_hex(&network.ssid, &network.password)
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
        wpa_psk_hex,
    };

    #[test]
    fn wifi_network_literal_escapes_rust_string_special_characters() {
        let psk = "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e";
        let network = FirmwareWifiNetwork {
            ssid:     "quoted\"\\ssid\n".to_string(),
            password: psk.to_string(),
        };

        assert_eq!(
            wifi_network_literal(&network),
            format!(
                "meteo_core::wifi_pick::Network {{ ssid: \"quoted\\\"\\\\ssid\\n\", psk_hex: \"{psk}\" }}"
            )
        );
    }

    /// Тест-векторы IEEE 802.11i-2004, приложение H.4.
    #[test]
    fn psk_matches_ieee_802_11i_vectors() {
        assert_eq!(
            wpa_psk_hex("IEEE", "password"),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
        assert_eq!(
            wpa_psk_hex("ThisIsASSID", "ThisIsAPassword"),
            "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af"
        );
    }

    #[test]
    fn hex_psk_in_config_passes_through() {
        let psk = "0DC0D6EB90555ED6419756B9A15EC3E3209B63DF707DD508D14581F8982721AF";
        assert_eq!(wpa_psk_hex("any", psk), psk.to_ascii_lowercase());
    }

    #[test]
    #[should_panic(expected = "8..=63")]
    fn short_passphrase_is_rejected() {
        let _ = wpa_psk_hex("ssid", "short");
    }
}
