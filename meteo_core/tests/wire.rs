use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use meteo_core::wire::{
    BUF_LEN,
    BaroReading,
    EpochMillis,
    LEN_PREFIX,
    MAX_BATCH,
    MAX_PAYLOAD_LEN,
    PacketCounter,
    PacketError,
    ScdReading,
    SensorBatch,
    SensorData,
    TAG_LEN,
    decode_payload,
    encode_packet,
    payload_len,
};

fn cipher() -> Aes128Gcm {
    Aes128Gcm::new(&(*b"supersecretkey!1").into())
}

/// Строка худшего размера: оба датчика есть, varint'ы максимальной длины.
fn worst_reading(i: u64) -> SensorData {
    SensorData {
        baro: Some(BaroReading {
            pressure: f32::MAX,
            temp:     f32::MIN,
        }),
        scd:  Some(ScdReading {
            co2:      u16::MAX,
            humidity: f32::MAX,
            temp:     f32::MIN,
        }),
        time: EpochMillis(u64::MAX - i),
    }
}

fn batch_of(n: usize, reading: impl Fn(u64) -> SensorData) -> SensorBatch {
    (0..n as u64).map(reading).collect()
}

/// Encode прошивки → разбор кадра → decode приёмника.
fn roundtrip(batch: &SensorBatch, counter: PacketCounter) -> Result<SensorBatch, PacketError> {
    let mut buf = [0u8; BUF_LEN];
    let len = encode_packet(&cipher(), counter, batch, &mut buf);
    let payload = payload_len(buf[..LEN_PREFIX].try_into().unwrap())?;
    assert_eq!(LEN_PREFIX + payload, len, "prefix must describe the whole packet");
    decode_payload(&cipher(), counter, &mut buf[LEN_PREFIX..len])
}

#[test]
fn worst_case_full_batch_roundtrips() {
    let batch = batch_of(MAX_BATCH, worst_reading);
    assert_eq!(roundtrip(&batch, PacketCounter::FIRST), Ok(batch));
}

#[test]
fn sparse_and_empty_batches_roundtrip() {
    let sparse = batch_of(3, |i| {
        SensorData {
            baro: None,
            scd:  (i % 2 == 0).then_some(ScdReading {
                co2:      400 + i as u16,
                humidity: 40.5,
                temp:     21.25,
            }),
            time: EpochMillis(1_757_986_271_840 + i),
        }
    });
    assert_eq!(roundtrip(&sparse, PacketCounter(7)), Ok(sparse));
    assert_eq!(
        roundtrip(&SensorBatch::new(), PacketCounter(8)),
        Ok(SensorBatch::new())
    );
}

#[test]
fn wrong_counter_fails_decrypt() {
    let batch = batch_of(2, worst_reading);
    let mut buf = [0u8; BUF_LEN];
    let len = encode_packet(&cipher(), PacketCounter(5), &batch, &mut buf);
    let res = decode_payload(&cipher(), PacketCounter(6), &mut buf[LEN_PREFIX..len]);
    assert_eq!(res, Err(PacketError::Decrypt));
}

#[test]
fn any_flipped_bit_fails_decrypt() {
    let batch = batch_of(2, worst_reading);
    let mut clean = [0u8; BUF_LEN];
    let len = encode_packet(&cipher(), PacketCounter::FIRST, &batch, &mut clean);
    for byte in LEN_PREFIX..len {
        let mut buf = clean;
        buf[byte] ^= 0x01;
        let res = decode_payload(&cipher(), PacketCounter::FIRST, &mut buf[LEN_PREFIX..len]);
        assert_eq!(res, Err(PacketError::Decrypt), "flipped byte {byte}");
    }
}

#[test]
fn payload_len_bounds() {
    let prefix = |n: u32| n.to_be_bytes();
    assert_eq!(payload_len(prefix(TAG_LEN as u32)), Ok(TAG_LEN));
    assert_eq!(payload_len(prefix(MAX_PAYLOAD_LEN as u32)), Ok(MAX_PAYLOAD_LEN));
    assert_eq!(
        payload_len(prefix(TAG_LEN as u32 - 1)),
        Err(PacketError::LengthOutOfRange(TAG_LEN - 1))
    );
    assert_eq!(
        payload_len(prefix(MAX_PAYLOAD_LEN as u32 + 1)),
        Err(PacketError::LengthOutOfRange(MAX_PAYLOAD_LEN + 1))
    );
    assert_eq!(
        payload_len(prefix(u32::MAX)),
        Err(PacketError::LengthOutOfRange(u32::MAX as usize))
    );
}

#[test]
fn short_payload_is_rejected_not_panicking() {
    for len in 0..TAG_LEN {
        let mut buf = vec![0u8; len];
        assert_eq!(
            decode_payload(&cipher(), PacketCounter::FIRST, &mut buf),
            Err(PacketError::LengthOutOfRange(len))
        );
    }
}
