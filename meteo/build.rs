#[allow(unused_imports)]
use std::{
    env, fs,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
};

use crate::build_helpers::secret_literal;

mod build_helpers;

fn main() {
    // linker_be_nice();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    // println!("cargo:rustc-link-arg=-Tlinkall.x");
    // println!("cargo:rustc-link-arg=-Tdefmt.x");

    let cargo_toml = fs::read_to_string("config.toml").unwrap();

    let fw = build_helpers::parse_config(&cargo_toml);

    // let ip_literal = ip_literal(fw.server_ip);
    let server_ip_literal = build_helpers::ipv4_literal(fw.server_ip);

    let secret_bytes_literal = secret_literal(&fw.secret_key);

    // генерим константу
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest_path = out_dir.join("constants.rs");

    // Слайс (ssid, passwd) в порядке из конфига: [0] — основная сеть, [1] — wifi2.
    // Прошивка сама выбирает между ними по RSSI, порядок здесь — только fallback.
    let networks_literal = fw
        .wifi_networks
        .iter()
        .map(|(ssid, passwd)| format!("(\"{ssid}\", \"{passwd}\")"))
        .collect::<Vec<_>>()
        .join(", ");

    let contents = format!(
        r#"
pub static WIFI_NETWORKS: &[(&str, &str)] = &[{networks}];
pub const SERVER_IP: core::net::Ipv4Addr = {ip};
pub const SERVER_PORT: u16 = {server_port};
pub static SECRET_KEY: [u8; 16] = [{secret}];
"#,
        networks = networks_literal,
        ip = server_ip_literal,
        server_port = fw.server_port,
        secret = secret_bytes_literal
    );

    fs::write(&dest_path, contents).unwrap();

    println!("cargo:warning==== Firmware build parameters ===");
    for (i, (ssid, passwd)) in fw.wifi_networks.iter().enumerate() {
        println!(
            "cargo:warning=WiFi #{}        : {} / {}",
            i + 1,
            ssid,
            passwd
        );
    }
    println!("cargo:warning=Server IP     : {}", fw.server_ip);
    println!("cargo:warning=Server PORT   : {}", fw.server_port);
    println!("cargo:warning=Secret Key    : {:?}", fw.secret_key);
    println!("cargo:warning===============================");

    println!("cargo:rerun-if-changed=config.toml");
}

#[allow(dead_code)]
fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                "_defmt_timestamp" => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                "esp_wifi_preempt_enable"
                | "esp_wifi_preempt_yield_task"
                | "esp_wifi_preempt_task_create" => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-wifi` has no scheduler enabled. Make sure you have the `builtin-scheduler` feature enabled, or that you provide an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "💡 `embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests"
                    );
                    eprintln!();
                }
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
