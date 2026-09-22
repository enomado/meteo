# example_server

A minimal reference receiver for the `meteo` firmware. It shows how to accept
the firmware's data, decrypt and decode it, print the readings and
acknowledge them — so you can plug in whatever backend you like (database,
queue, file, another service…).

```sh
cargo run                          # listens on 0.0.0.0:1234 (UDP and TCP)
METEO_LISTEN=0.0.0.0:5000 cargo run
METEO_LIVE_SECS=300 cargo run      # ask the firmware for live mode in every ack
```

Point the firmware at this server's IP/port (`meteo/config.toml`,
`server_ip`/`server_port`) and use the same 16-byte `secret_key`.

## Protocol v2 (UDP, current firmware)

Each datagram (at most 1172 bytes, so it is never IP-fragmented):

```text
[ 9-byte header ][ AES-128-GCM ciphertext ][ 16-byte tag ]
```

- Header (big-endian, authenticated as AES-GCM associated data): `b0` —
  version in bits 7..4, direction in bit 3, `ACK_REQ` in bit 0; then
  `boot id + sequence number` (firmware → server) or
  `server salt + ack number` (server → firmware). The nonce is built from
  the header, so lost, reordered or duplicated datagrams do not affect
  decryption.
- Plaintext: readings in fixed point (pressure 0.01 Pa, temperatures
  0.001 °C, humidity 0.001 %, CO2 ppm), each channel optional, delta- and
  varint-encoded within one datagram (`meteo_core/src/codec.rs`).
- A datagram with `ACK_REQ` is answered **after** its readings are stored:
  the ack carries which sequence numbers of that boot are stored (a 32-bit
  window) and how many seconds of live mode the firmware should keep (send
  each reading immediately instead of batching every 120 s). Readings the
  window does not confirm are resent in a new datagram, so storage must be
  idempotent by reading time.

The receiver logic — verification, duplicate window, ack — is in
`../meteo_core/src/server.rs` (feature `std`), shared with the firmware's
own tests; `src/main.rs` is only the socket loop. To store readings, replace
the `>>> Plug your backend here` section and call `ingested` only after the
store succeeded.

## Protocol v1 (TCP, older firmware)

Still accepted until old firmware is retired:
`[ u32 BE payload_len ][ AES-128-GCM ciphertext || tag ]` per packet,
postcard-encoded `SensorData` batch (`../meteo_core/src/wire.rs`), nonce from
a per-connection counter. The counter restarts on every reconnect, so the
nonce repeats — v1 is not transport security.
