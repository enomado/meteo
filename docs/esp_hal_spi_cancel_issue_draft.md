# DRAFT — not filed. Target: github.com/esp-rs/esp-hal

**Title:** SPI master (async, FIFO): cancelling `SpiBus::read`/`write`/`transfer` leaves the transfer running; only `transfer_in_place` aborts

## Summary

For `Spi<'_, Async>` without DMA, the async `embedded_hal_async::spi::SpiBus`
methods do not agree on what happens when the future is dropped mid-chunk:

| method | on drop |
|---|---|
| `transfer_in_place` | aborts the chunk and blocks until idle (`cancel_on_drop`, #3242) |
| `half_duplex_read_async` / `half_duplex_write_async` | abort the same way |
| `read`, `write`, `transfer` | **do not abort**: the current FIFO chunk keeps clocking in the background |

`#3242` fixed exactly this problem for `transfer_in_place` ("the transfer
completed in the background, even if a SpiDevice wrapper has deasserted the
chip select line during the cancellation"). The FIFO `read_async` /
`write_async` / `transfer_async` paths added later (#5247) did not get the guard.

Code at `8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13` (current `main`):

- `write_async` / `read_async`: `write_one` + `flush_async().await`, no guard —
  [low_level/mod.rs#L384-L390](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L384-L390),
  [#L414-L423](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L414-L423)
- `transfer_async`: same, no guard —
  [#L799-L834](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L799-L834)
- `SpiFuture::drop` only disables the interrupt —
  [#L1095-L1099](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L1095-L1099)
- compare `transfer_in_place_async` —
  [#L779-L796](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L779-L796)
  and the half-duplex guards
  [#L680-L683](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L680-L683)
- `SpiBusAsync` impl routes `read`/`write`/`transfer` to the unguarded paths —
  [mod.rs#L1424-L1459](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L1424-L1459)

## Consequences

1. **CS is deasserted while SCLK still runs.** `embassy-embedded-hal`'s
   `SpiDevice` deasserts CS from an `OnDrop` guard on cancellation
   ([shared_bus/asynch/spi.rs#L74-L80, 0.6.0](https://docs.rs/embassy-embedded-hal/0.6.0/src/embassy_embedded_hal/shared_bus/asynch/spi.rs.html#74-80)).
   With `embassy_time::with_timeout(.., dev.write(..))` firing mid-chunk, CS goes
   high and the bus mutex is released while up to `FIFO_SIZE` (64) bytes are
   still being shifted out. A second device on the same bus can assert its CS
   during that tail.

2. **The next operation reprograms a busy peripheral.** Nothing waits for the
   leftover transfer before the registers are touched again:
   - every entry point calls `setup_full_duplex()` (modifies `USER`) first —
     [mod.rs#L952](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L952),
     [#L964](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L964),
     [#L1436](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L1436);
   - `read_async`/`write_async` (and the blocking `read`/`write`) then call
     `write_one()` — `MS_DLEN`, `W0..W15`, `CMD.USR` — without a prior busy
     check. `transfer_async` does `flush_async().await` before `write_one`
     ([#L813](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/low_level/mod.rs#L813)),
     but only after `setup_full_duplex()`.
   - Since #5836, `SpiBus::flush` and `Spi::flush_async` are no-ops
     ([mod.rs#L1461-L1463](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L1461-L1463),
     [#L830-L832](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L830-L832)),
     so a caller cannot wait for the leftover chunk either. "Every transfer flushes
     before returning" holds only if the transfer is not cancelled.

3. **Clock / wake lock released under a running transfer (from reading the code,
   not observed).** The per-call `SpiClockGuard`
   ([mod.rs#L950](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L950),
   [#L1434](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/master/mod.rs#L1434))
   drops with the cancelled future. That releases the SPI function clock and a
   `WakeLock` while the shifter is still busy. On ESP32-C3 the SPI-level gate is
   a no-op
   ([spi/clocks/v3.rs#L6](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/clocks/v3.rs#L6)).
   On PCR chips it clears `SPI2_CLKM_EN`
   ([spi/clocks/v3_pcr.rs#L9-L18](https://github.com/esp-rs/esp-hal/blob/8ef1b6716f48cfe34f7e0dd8cbcf738ce236da13/esp-hal/src/spi/clocks/v3_pcr.rs#L9-L18)),
   which presumably freezes the chunk with `USR` still set until the next call
   re-enables the clock.

## Expected behaviour

Either of:

- the same `cancel_on_drop` guard (`abort_transfer()` + blocking `flush()`) in
  `write_async`, `read_async` and `transfer_async`, so all FIFO paths stop the
  transfer on drop, as `transfer_in_place` already does (preferred: it is what
  `SpiDevice` implementations relying on `OnDrop` for CS assume); or
- documentation on the `SpiBus` impl saying that cancelling `read`/`write`/`transfer`
  leaves the chunk running and the next call may reconfigure a busy peripheral.

## Environment

- esp-hal `main` @ `8ef1b671` (1.2.0), ESP32-C3 (`riscv32imc`)
- `Spi<'static, Async>` from `Spi::new(..).into_async()`, **no DMA**,
  `min_async_transfer_size` = 0 (default)
- `embassy-embedded-hal` 0.6.0 `shared_bus::asynch::spi::SpiDevice` over
  `embassy_sync::mutex::Mutex<NoopRawMutex, Spi<Async>>`, CS = `gpio::Output`

## Minimal reproduction (sketch, NOT yet run on hardware)

Slow clock so one 64-byte chunk takes ~5 ms. Watch SCLK and both CS lines on a
logic analyser.

```rust
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embassy_time::{Duration, with_timeout};
use embedded_hal_async::spi::SpiDevice as _;
use esp_hal::{
    gpio::{Level, Output, OutputConfig},
    spi::master::{Config, Spi},
    time::Rate,
};

// inside an async main / task:
let spi = Spi::new(peripherals.SPI2, Config::default().with_frequency(Rate::from_khz(100)))
    .unwrap()
    .with_sck(peripherals.GPIO6)
    .with_mosi(peripherals.GPIO7)
    .with_miso(peripherals.GPIO2)
    .into_async();
let bus: Mutex<NoopRawMutex, _> = Mutex::new(spi);
let mut dev_a = SpiDevice::new(&bus, Output::new(peripherals.GPIO10, Level::High, OutputConfig::default()));
let mut dev_b = SpiDevice::new(&bus, Output::new(peripherals.GPIO3, Level::High, OutputConfig::default()));

// 64 bytes @ 100 kHz ≈ 5.1 ms; cancel after 1 ms.
let r = with_timeout(Duration::from_millis(1), dev_a.write(&[0xAA; 64])).await;
assert!(r.is_err()); // timed out

// Expected (same as transfer_in_place): SCLK stops at ~1 ms together with CS_A.
// Suspected: CS_A rises at ~1 ms, SCLK keeps running until ~5.1 ms.

// Immediately start another device's transaction on the still-busy peripheral.
dev_b.write(&[0x55; 4]).await.unwrap();
// Suspected: CS_B asserts while the 0xAA tail is still clocking; dev_b sees
// extra clocks / wrong data.

// Control: the same with `dev_a.transfer_in_place(&mut [0xAA; 64])` should show
// SCLK stopping at ~1 ms.
```

## Related

- #3242 — the same fix for `transfer_in_place` (merged)
- #2216 — DMA transfers abort on drop
- #5247 — introduced the current FIFO `read_async`/`write_async`/`transfer_async`
- #5836 — made `flush` a no-op and per-transfer clock guards
- #5515 — open, DMA-side drop semantics (different path)
