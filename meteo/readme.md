# meteo

ESP32-C3 weather station in Rust (esp-hal, embassy).

## Dependencies

```sh
rustup toolchain install nightly
rustup target add riscv32imc-unknown-none-elf
cargo install espflash
```

## Configuration

Copy `config.toml.template` to `config.toml` and fill in:

```toml
[firmware]
wifi_ssid = "..."
wifi_passwd = "..."
# optional second network (both keys or neither)
wifi2_ssid = "..."
wifi2_passwd = "..."
secret_key = "..."  # 16-byte hex (128-bit AES-GCM key)
server_ip = "..."
server_port = 1234
```

With `wifi2_*` set, the firmware scans before every (re)connect and joins whichever
of the two SSIDs has the stronger RSSI. If the scan fails or neither SSID is on air,
it alternates between the configured networks on successive attempts. With a single
network configured, no scan is performed.

## Build & Flash

```sh
# build + flash + open serial monitor
cargo run --release

# build only
cargo build --release
```

`cargo run` uses the runner from `.cargo/config.toml`:
```
espflash flash --monitor --chip esp32c3
```

`espflash` will auto-detect the USB port. If multiple devices are connected, specify the port explicitly:
```sh
espflash flash --monitor --chip esp32c3 --port /dev/ttyUSB0 target/riscv32imc-unknown-none-elf/release/meteo
```

## Tests

This crate builds only for riscv. Hardware-independent logic — wire format,
LED palette, watchdog decision, WiFi choice, the packet sender state machine —
lives in [`../meteo_core`](../meteo_core) and is tested on the host:

```sh
cd ../meteo_core && cargo test --release
```

The sender test is a deterministic simulation (fixed-seed proptest) of partial
writes, connection drops and outages; it checks that no reading is lost
without being counted as evicted from a full queue.

## SPI Pinout (BMP390)

| ESP32-C3 | Signal | Pin    |
|----------|--------|--------|
| IO14     | CS     | SPICS0 |
| IO15     | CLK    | SPICLK |
| IO16     | MISO   | SPID   |
| IO17     | MOSI   | SPIQ   |

> IO13, IO14 are reserved for debugging — do not use.

## Links

- [ESP32-C3 TRM](https://www.espressif.com/sites/default/files/documentation/esp32-c3_technical_reference_manual_en.pdf#iomuxgpio)
- [Hardware design guidelines](https://docs.espressif.com/projects/esp-hardware-design-guidelines/en/latest/esp32c3/schematic-checklist.html#fig-rf-tuning)
- [Dev board](https://botland.store/withdrawn-products/21026-esp-c3-32s-kit-wifi-bluetooth-development-board-with-esp-c3-32s-module.html)
