# Atmospheric Pressure Logger

A simple atmospheric pressure logger built with **Rust**, running on **ESP32-C3** using [**esp-hal**](https://github.com/esp-rs/esp-hal).

---

## ✨ Features
- 🦀 Written in **Rust** for safety and performance  
- ⚡ Runs on **ESP32-C3** microcontroller  
- 🔌 Uses [**esp-hal**](https://github.com/esp-rs/esp-hal) for hardware abstraction  
- 🌡️ Supports sensors like **BMP390** via [**bmp390-rs**](https://github.com/yourname/bmp390-rs)  
- ⏱️ Configurable sampling interval (from seconds to hours)  
- 💾 Stores data to a **remote server via Wi-Fi**  
- 🌐 Syncs time using **NTP**

---

## 🙏 Credits

This project wouldn’t be possible without these fantastic open-source libraries:

- [**bmp390-rs**](https://github.com/yourname/bmp390-rs) – sensor driver support
- [**sntpc**](https://crates.io/crates/sntpc) – SNTP client for time synchronization  
- [**esp-hal**](https://github.com/esp-rs/esp-hal) – an amazing ESP32 hardware abstraction layer
- [**embassy**](https://github.com/embassy-rs/embassy) – Asynchronous embedded framework  

Many thanks to the authors and maintainers of these crates for their hard work!

## 📜 License

This project is licensed under the [MIT License](https://opensource.org/licenses/MIT).

## 🔗 Useful Links

- [Waveshare ESP-C3-32S Kit](https://www.waveshare.com/wiki/ESP-C3-32S-Kit)  
- [Example project: esp32c3-embassy](https://github.com/claudiomattera/esp32c3-embassy?tab=readme-ov-file)  
- [ESP32 Pinout Guide](https://esp32.implrust.com/esp32-intro/pinout.html)  
- [ESP32 Buyer’s Guide](https://eitherway.io/posts/esp32-buyers-guide/)  
- [Espressif chip comparison](https://docs.espressif.com/projects/esp-idf/en/v5.0/esp32s3/hw-reference/chip-series-comparison.html)  
- [Embassy on ESP32](https://esp32.implrust.com/embassy/index.html)  
- [esp-hal SNTP Example](https://github.com/esp-rs/esp-hal/blob/bea71a18842a0fc097534a7cf3890b756df131e2/examples/wifi/sntp/Cargo.toml#L1)  
- [Ticker in `embassy-time`](https://docs.rs/embassy-time/latest/embassy_time/struct.Ticker.html)
