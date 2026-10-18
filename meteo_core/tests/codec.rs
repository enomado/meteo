use meteo_core::codec::{
    CentiPascal,
    CodecError,
    Encoder,
    EpochMillis,
    HEALTH_MAX,
    MIN_PLAIN,
    MIN_PLAIN_WITH_HEALTH,
    MilliCelsius,
    MilliPercentRh,
    Ppm,
    READING_MAX,
    Reading,
    Record,
    decode,
};
use meteo_core::datagram::PLAIN_CAPACITY;
use meteo_core::health::{
    LAYOUT,
    SNAPSHOT_WORDS,
    Snapshot,
};
use proptest::collection::vec;
use proptest::prelude::*;

fn full(time: u64, i: i32, co2: u16) -> Reading {
    Reading {
        time:      EpochMillis(time),
        pressure:  Some(CentiPascal(i)),
        baro_temp: Some(MilliCelsius(i)),
        co2:       Some(Ppm(co2)),
        humidity:  Some(MilliPercentRh(i)),
        scd_temp:  Some(MilliCelsius(i)),
    }
}

/// Кодирует сколько влезет в буфер `cap`; возвращает текст и число показаний.
fn encode(readings: &[Reading], cap: usize) -> (Vec<u8>, usize) {
    let mut buf = vec![0u8; cap];
    let mut enc = Encoder::new(&mut buf, readings[0].time);
    let taken = readings.iter().take_while(|r| enc.push(r)).count();
    let len = enc.plain_len();
    buf.truncate(len);
    (buf, taken)
}

fn decode_all(plain: &[u8]) -> Result<Vec<Reading>, CodecError> {
    decode(plain)?
        .map(|record| {
            record.map(|record| {
                match record {
                    Record::Reading(r) => r,
                    Record::Health(h) => panic!("unexpected health record {h:?}"),
                }
            })
        })
        .collect()
}

#[test]
fn worst_reading_is_exactly_reading_max() {
    // Второе показание: время прыгает на i64::MIN, каналы — через весь диапазон.
    let a = full(u64::MAX, i32::MAX, 0);
    let b = full(u64::MAX / 2, i32::MIN, u16::MAX);
    let (one, _) = encode(&[a], PLAIN_CAPACITY);
    let (two, taken) = encode(&[a, b], PLAIN_CAPACITY);
    assert_eq!(taken, 2);
    assert_eq!(two.len() - one.len(), READING_MAX);
    assert_eq!(decode_all(&two), Ok(vec![a, b]));
}

#[test]
fn zigzag_handles_i64_minimum_delta() {
    let a = full(u64::MAX / 2, 0, 0);
    let b = full(0, 0, 0);
    let (plain, taken) = encode(&[a, b], PLAIN_CAPACITY);

    assert_eq!(taken, 2);
    assert_eq!(decode_all(&plain), Ok(vec![a, b]));
}

#[test]
fn greedy_fill_stops_before_overflow() {
    let worst: Vec<Reading> = (0..200)
        .map(|i| {
            if i % 2 == 0 {
                full(u64::MAX, i32::MAX, 0)
            } else {
                full(u64::MAX / 2, i32::MIN, u16::MAX)
            }
        })
        .collect();
    let (plain, taken) = encode(&worst, PLAIN_CAPACITY);
    assert!(plain.len() <= PLAIN_CAPACITY);
    assert!(taken >= (PLAIN_CAPACITY - MIN_PLAIN) / READING_MAX);
    assert_eq!(decode_all(&plain), Ok(worst[..taken].to_vec()));

    // Буфер ровно под одно худшее показание.
    let (plain, taken) = encode(&worst, MIN_PLAIN);
    assert_eq!(taken, 1);
    assert_eq!(decode_all(&plain), Ok(worst[..1].to_vec()));
}

#[test]
fn realistic_reading_is_small() {
    // Типовой цикл 30 с: мелкие приращения ⇒ показание ≈ 12–15 Б.
    let base = 1_758_000_000_000;
    let readings: Vec<Reading> = (0..4)
        .map(|index| {
            let index_i32 = i32::try_from(index).expect("the test uses four readings");
            let co2_index = u16::try_from(index).expect("the test uses four readings");
            Reading {
                time:      EpochMillis(base + index * 30_012),
                pressure:  Some(CentiPascal(10_132_500 + index_i32 * 37)),
                baro_temp: Some(MilliCelsius(21_500 - index_i32 * 11)),
                co2:       Some(Ppm(812 + co2_index * 5)),
                humidity:  Some(MilliPercentRh(45_120 + index_i32 * 150)),
                scd_temp:  Some(MilliCelsius(22_310 + index_i32 * 9)),
            }
        })
        .collect();
    let (plain, _) = encode(&readings, PLAIN_CAPACITY);
    let (first, _) = encode(&readings[..1], PLAIN_CAPACITY);
    let per_next = (plain.len() - first.len()) / 3;
    assert!(per_next <= 15, "{per_next} bytes per follow-up reading");
    assert_eq!(decode_all(&plain), Ok(readings));
}

#[test]
fn absent_channels_do_not_break_deltas() {
    let t = 1_758_000_000_000;
    let mut with_missing_humidity = full(t, 1000, 400);
    with_missing_humidity.humidity = None;
    let mut with_missing_pressure_and_co2 = full(t + 1, 2000, 500);
    with_missing_pressure_and_co2.pressure = None;
    with_missing_pressure_and_co2.co2 = None;
    let stepped_back = full(t - 5000, -3, 600); // NTP-пересинхронизация
    let empty = Reading {
        time:      EpochMillis(t + 7),
        pressure:  None,
        baro_temp: None,
        co2:       None,
        humidity:  None,
        scd_temp:  None,
    };
    let (plain, taken) = encode(
        &[
            with_missing_humidity,
            with_missing_pressure_and_co2,
            stepped_back,
            empty,
        ],
        PLAIN_CAPACITY,
    );
    assert_eq!(taken, 4);
    assert_eq!(
        decode_all(&plain),
        Ok(vec![
            with_missing_humidity,
            with_missing_pressure_and_co2,
            stepped_back,
            empty,
        ])
    );
}

#[test]
fn malformed_plaintext_is_an_error() {
    assert_eq!(decode_all(&[]).err(), Some(CodecError::Truncated));
    assert_eq!(decode_all(&[0x80]).err(), Some(CodecError::Truncated));
    // 11 байт с продолжением — длиннее 64 бит.
    assert_eq!(decode_all(&[0xFF; 11]).err(), Some(CodecError::VarintOverflow));
    // Десятый байт несёт больше 64-го бита.
    let mut too_big = [0xFF; 10];
    too_big[9] = 0x02;
    assert_eq!(decode_all(&too_big).err(), Some(CodecError::VarintOverflow));
    // base 0, присутствие с битом 5.
    assert_eq!(
        decode_all(&[0, 0x20]).err(),
        Some(CodecError::ReservedPresence(0x20))
    );
    // base 0, co2 (бит 2) = −1.
    assert_eq!(
        decode_all(&[0, 0x04, 0, 0x01]).err(),
        Some(CodecError::OutOfRange)
    );
    // base 0, pressure: дельта 2³¹ от нуля — вне i32.
    assert_eq!(
        decode_all(&[0, 0x01, 0, 0x80, 0x80, 0x80, 0x80, 0x10]).err(),
        Some(CodecError::OutOfRange)
    );
    // Показание оборвано посреди каналов.
    assert_eq!(decode_all(&[0, 0x03, 0, 0x02]).err(), Some(CodecError::Truncated));
}

fn snapshot(seed: u32) -> Snapshot {
    let mut words = [0u32; SNAPSHOT_WORDS];
    for (i, w) in words.iter_mut().enumerate() {
        *w = seed.wrapping_mul(u32::try_from(i).unwrap() + 1);
    }
    words[SNAPSHOT_WORDS - 1] = u32::MAX;
    Snapshot(words)
}

/// Снимок первым, показания за ним: цепочка дельт показаний не тронута.
#[test]
fn health_record_roundtrips_before_readings() {
    let a = full(1_758_000_000_000, 100, 800);
    let b = full(1_758_000_030_000, 105, 812);
    let snap = snapshot(0x1234_5678);
    let mut buf = vec![0u8; PLAIN_CAPACITY];
    let mut enc = Encoder::new(&mut buf, a.time);
    assert!(enc.push_health(&snap));
    assert!(enc.push(&a));
    assert!(enc.push(&b));
    let len = enc.plain_len();
    let records: Vec<Record> = decode(&buf[..len]).unwrap().collect::<Result<_, _>>().unwrap();
    let Record::Health(h) = &records[0] else {
        panic!("first record is {:?}", records[0])
    };
    assert_eq!(h.layout, LAYOUT);
    assert_eq!(Snapshot::from_record(h), Some(snap));
    assert_eq!(records[1..], [Record::Reading(a), Record::Reading(b)]);
}

/// Датаграмма из одного снимка (датчики мертвы): база времени — ноль.
#[test]
fn health_only_plaintext_decodes() {
    let snap = snapshot(7);
    let mut buf = vec![0u8; MIN_PLAIN_WITH_HEALTH];
    let mut enc = Encoder::new(&mut buf, EpochMillis(0));
    assert!(enc.push_health(&snap));
    let len = enc.plain_len();
    assert!(len <= 1 + HEALTH_MAX);
    let records: Vec<Record> = decode(&buf[..len]).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(records.len(), 1);
    let Record::Health(h) = &records[0] else {
        panic!("not a health record")
    };
    assert_eq!(Snapshot::from_record(h), Some(snap));
}

#[test]
fn foreign_layout_is_kept_raw() {
    // base 0, снимок раскладки 99 из двух слов.
    let records: Vec<Record> = decode(&[0, 0x80, 99, 2, 5, 6])
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let Record::Health(h) = &records[0] else {
        panic!("not a health record")
    };
    assert_eq!((h.layout, h.words.as_slice()), (99, &[5, 6][..]));
    assert_eq!(Snapshot::from_record(h), None);
}

#[test]
fn malformed_extended_records_are_errors() {
    assert_eq!(
        decode(&[0, 0x81]).unwrap().next(),
        Some(Err(CodecError::UnknownRecord(0x81)))
    );
    // n = 97 > HEALTH_WORDS_MAX.
    assert_eq!(
        decode(&[0, 0x80, 1, 97]).unwrap().next(),
        Some(Err(CodecError::HealthTooLong(97)))
    );
    // Слово вне u32: 2³².
    assert_eq!(
        decode(&[0, 0x80, 1, 1, 0x80, 0x80, 0x80, 0x80, 0x10])
            .unwrap()
            .next(),
        Some(Err(CodecError::OutOfRange))
    );
    // Оборвано посреди слов.
    assert_eq!(
        decode(&[0, 0x80, 1, 3, 5]).unwrap().next(),
        Some(Err(CodecError::Truncated))
    );
    // Бит 6 у байта присутствия по-прежнему зарезервирован.
    assert_eq!(
        decode_all(&[0, 0x40]).err(),
        Some(CodecError::ReservedPresence(0x40))
    );
}

#[test]
fn fixed_point_conversion() {
    assert_eq!(
        CentiPascal::from_pascal(101_325.37),
        Some(CentiPascal(10_132_537))
    );
    assert_eq!(MilliCelsius::from_celsius(-12.3456), Some(MilliCelsius(-12_346)));
    assert_eq!(MilliPercentRh::from_percent(45.0), Some(MilliPercentRh(45_000)));
    assert_eq!(MilliCelsius::from_celsius(f32::NAN), None);
    assert_eq!(MilliCelsius::from_celsius(f32::INFINITY), None);
    assert_eq!(CentiPascal::from_pascal(3.0e7), None, "beyond i32 at 0.01 Pa");
    assert_eq!(CentiPascal(10_132_537).pascal(), 101_325.37);
}

fn reading() -> impl Strategy<Value = Reading> {
    (
        any::<u64>(),
        proptest::option::of(any::<i32>()),
        proptest::option::of(any::<i32>()),
        proptest::option::of(any::<u16>()),
        proptest::option::of(any::<i32>()),
        proptest::option::of(any::<i32>()),
    )
        .prop_map(|(time, pressure, baro_temp, co2, humidity, scd_temp)| {
            Reading {
                time:      EpochMillis(time),
                pressure:  pressure.map(CentiPascal),
                baro_temp: baro_temp.map(MilliCelsius),
                co2:       co2.map(Ppm),
                humidity:  humidity.map(MilliPercentRh),
                scd_temp:  scd_temp.map(MilliCelsius),
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn roundtrip_any_readings(readings in vec(reading(), 1..120)) {
        let (plain, taken) = encode(&readings, PLAIN_CAPACITY);
        prop_assert!(plain.len() <= PLAIN_CAPACITY);
        prop_assert!(taken >= 1);
        prop_assert_eq!(decode_all(&plain), Ok(readings[..taken].to_vec()));
    }

    #[test]
    fn decoder_never_panics(bytes in vec(any::<u8>(), 0..300)) {
        let _ = decode_all(&bytes);
    }
}
