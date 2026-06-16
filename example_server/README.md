# example_server

A minimal reference receiver for the `meteo` firmware. It shows how to accept
the firmware's TCP stream, decrypt and decode each packet, and print the
readings — so you can plug in whatever backend you like (database, queue,
file, another service…).

```sh
cargo run            # listens on 0.0.0.0:1234
METEO_LISTEN=0.0.0.0:5000 cargo run
```

Point the firmware at this server's IP/port (`meteo/config.toml`,
`server_ip`/`server_port`) and use the same 16-byte `secret_key`.

## Wire protocol

Each packet on the TCP stream:

```text
[ u32 big-endian payload_len ][ AES-128-GCM ciphertext || 16-byte tag ]
```

- `payload_len` = ciphertext length + 16 (GCM tag appended inline).
- AES-128-GCM, 16-byte shared key (must match the firmware's `secret_key`),
  empty associated data.
- Nonce: 4 zero bytes + 8-byte big-endian packet counter, starting at 1 per
  connection and incrementing per packet (the firmware resets it on reconnect).
- Plaintext: a postcard-encoded `Vec<SensorData>`. Field order must match the
  firmware (`meteo/src/sensor.rs`) — postcard is order-based, names are not sent.

`SensorData` is sparse: `baro` (BMP390: pressure Pa, temp °C) and `scd` (SCD41:
CO2 ppm, humidity %, temp °C) are each `Option`, and `time` is ms since the Unix
epoch. See `src/main.rs` for the exact structs.

> The counter-based nonce repeats across firmware reboots — this is an
> intentional simplification of a hobby protocol, not transport security.

To build a real backend, copy this crate and replace the `>>> Plug your backend
here` section in `src/main.rs`.
