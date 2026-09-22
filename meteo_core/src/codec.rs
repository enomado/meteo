//! Кодек показаний протокола v2: фиксированная точка + дельты + varint.
//!
//! Открытый текст датаграммы: `base_time` (varint), затем показания до конца
//! (счётчика нет). Показание: байт присутствия (биты 0–4 — pressure,
//! baro_temp, co2, humidity, scd_temp; биты 5–7 = 0), `dt` мс от предыдущего
//! показания (первое — от `base_time`; zigzag varint, время после
//! пересинхронизации NTP может идти назад), затем для каждого присутствующего
//! канала — zigzag varint дельта от предыдущего присутствующего значения
//! ЭТОГО канала.
//!
//! Состояние дельт живёт только внутри одной датаграммы (старт — нули ⇒ первое
//! значение абсолютное): потеря датаграммы не портит следующие.

use crate::wire::EpochMillis;

/// Давление, 0.01 Па (≈ шаг f32 около 100 кПа).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CentiPascal(pub i32);

/// Температура, 0.001 °C (шаг SCD41 — 0.0027 °C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MilliCelsius(pub i32);

/// Концентрация CO2, ppm — как её отдаёт SCD41.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ppm(pub u16);

/// Относительная влажность, 0.001 % (шаг SCD41 — 0.0015 %).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MilliPercentRh(pub i32);

/// `value × scale`, округлённое до целого. `None` — нефинитное значение или
/// вне i32: на провод такое не кладём. Умножение в f64 точное (24 бита
/// мантиссы f32 × масштаб ≤ 10 бит < 53), округление — одно.
fn scaled(value: f32, scale: f64) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    let v = libm::round(f64::from(value) * scale);
    (f64::from(i32::MIN)..=f64::from(i32::MAX))
        .contains(&v)
        .then_some(v as i32)
}

impl CentiPascal {
    const PER_PASCAL: f64 = 100.0;

    pub fn from_pascal(pa: f32) -> Option<Self> {
        scaled(pa, Self::PER_PASCAL).map(Self)
    }

    pub fn pascal(self) -> f64 {
        f64::from(self.0) / Self::PER_PASCAL
    }
}

impl MilliCelsius {
    const PER_CELSIUS: f64 = 1000.0;

    pub fn from_celsius(c: f32) -> Option<Self> {
        scaled(c, Self::PER_CELSIUS).map(Self)
    }

    pub fn celsius(self) -> f64 {
        f64::from(self.0) / Self::PER_CELSIUS
    }
}

impl MilliPercentRh {
    const PER_PERCENT: f64 = 1000.0;

    pub fn from_percent(rh: f32) -> Option<Self> {
        scaled(rh, Self::PER_PERCENT).map(Self)
    }

    pub fn percent(self) -> f64 {
        f64::from(self.0) / Self::PER_PERCENT
    }
}

/// Одно показание. Каждый канал может отсутствовать сам по себе: датчика нет,
/// он не ответил в этом цикле или значение не перевелось в фиксированную точку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub time:      EpochMillis,
    pub pressure:  Option<CentiPascal>,
    pub baro_temp: Option<MilliCelsius>,
    pub co2:       Option<Ppm>,
    pub humidity:  Option<MilliPercentRh>,
    pub scd_temp:  Option<MilliCelsius>,
}

/// Число каналов = число битов присутствия.
const CHANNELS: usize = 5;
/// Биты 5–7 байта присутствия.
const RESERVED_PRESENCE: u8 = !((1 << CHANNELS) - 1);

impl Reading {
    /// Есть ли хоть один канал. Пустое показание прошивка в очередь не кладёт.
    pub fn has_any_channel(&self) -> bool {
        self.channels().iter().any(Option::is_some)
    }

    /// Каналы в порядке битов присутствия, значения — в i64 для дельт.
    fn channels(&self) -> [Option<i64>; CHANNELS] {
        [
            self.pressure.map(|v| i64::from(v.0)),
            self.baro_temp.map(|v| i64::from(v.0)),
            self.co2.map(|v| i64::from(v.0)),
            self.humidity.map(|v| i64::from(v.0)),
            self.scd_temp.map(|v| i64::from(v.0)),
        ]
    }

    fn from_channels(time: EpochMillis, ch: [Option<i64>; CHANNELS]) -> Result<Reading, CodecError> {
        fn narrow<T: TryFrom<i64>>(v: Option<i64>) -> Result<Option<T>, CodecError> {
            v.map(|v| T::try_from(v).map_err(|_| CodecError::OutOfRange))
                .transpose()
        }
        Ok(Reading {
            time,
            pressure: narrow(ch[0])?.map(CentiPascal),
            baro_temp: narrow(ch[1])?.map(MilliCelsius),
            co2: narrow(ch[2])?.map(Ppm),
            humidity: narrow(ch[3])?.map(MilliPercentRh),
            scd_temp: narrow(ch[4])?.map(MilliCelsius),
        })
    }
}

const fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

const fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Длина LEB128-varint для `v`.
const fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Худшая длина varint u64 (и zigzag i64).
const VARINT_U64_MAX: usize = varint_len(u64::MAX);
/// Худшая дельта i32-канала: |d| ≤ 2³² − 1.
const DELTA_I32_MAX: usize = varint_len(zigzag(i32::MAX as i64 - i32::MIN as i64));
/// Худшая дельта CO2 (u16): |d| ≤ 65 535.
const DELTA_U16_MAX: usize = varint_len(zigzag(u16::MAX as i64));

/// Худшее показание: присутствие + dt + четыре i32-канала + CO2.
pub const READING_MAX: usize = 1 + VARINT_U64_MAX + 4 * DELTA_I32_MAX + DELTA_U16_MAX;

/// Сколько места нужно кодировщику, чтобы влезло хотя бы одно показание.
pub const MIN_PLAIN: usize = VARINT_U64_MAX + READING_MAX;

// Хотя бы одно показание худшего размера влезает в любую датаграмму ⇒ отправитель
// всегда продвигается, даже если в бэклоге только худшие показания.
const _: () = assert!(MIN_PLAIN <= crate::datagram::PLAIN_CAPACITY);

/// Жадный кодировщик открытого текста одной датаграммы. Пишет в `buf`
/// без аллокаций; показание добавляется, пока остаток ≥ `READING_MAX`.
pub struct Encoder<'a> {
    buf:       &'a mut [u8],
    len:       usize,
    prev_time: EpochMillis,
    prev:      [i64; CHANNELS],
}

impl<'a> Encoder<'a> {
    /// `base` — время первого показания датаграммы (его `dt` тогда 0).
    pub fn new(buf: &'a mut [u8], base: EpochMillis) -> Self {
        assert!(
            buf.len() >= MIN_PLAIN,
            "encoder buffer of {} bytes fits no reading",
            buf.len()
        );
        let mut enc = Self {
            buf,
            len: 0,
            prev_time: base,
            prev: [0; CHANNELS],
        };
        enc.put_varint(base.0);
        enc
    }

    /// Длина открытого текста.
    pub fn plain_len(&self) -> usize {
        self.len
    }

    /// Добавить показание. `false` — места под худший случай нет, показание не
    /// записано (датаграмма заполнена).
    pub fn push(&mut self, r: &Reading) -> bool {
        if self.buf.len() - self.len < READING_MAX {
            return false;
        }
        let channels = r.channels();
        let presence =
            channels.iter().enumerate().fold(
                0u8,
                |acc, (bit, ch)| if ch.is_some() { acc | (1 << bit) } else { acc },
            );
        self.buf[self.len] = presence;
        self.len += 1;

        // Разность u64 в дополнительном коде: декодер делает wrapping_add ⇒
        // точный обратный путь для любых двух времён.
        let dt = (r.time.0 as i64).wrapping_sub(self.prev_time.0 as i64);
        self.put_varint(zigzag(dt));
        self.prev_time = r.time;

        for (ch, value) in channels.into_iter().enumerate() {
            if let Some(v) = value {
                self.put_varint(zigzag(v - self.prev[ch]));
                self.prev[ch] = v;
            }
        }
        true
    }

    fn put_varint(&mut self, mut v: u64) {
        // Место проверено до записи (`new` / `push` под READING_MAX).
        while v >= 0x80 {
            self.buf[self.len] = (v as u8) | 0x80;
            self.len += 1;
            v >>= 7;
        }
        self.buf[self.len] = v as u8;
        self.len += 1;
    }
}

/// Почему открытый текст не разбирается. Прошивка такого не шлёт: это порча
/// или чужая реализация.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// Текст кончился посреди значения.
    Truncated,
    /// Varint длиннее 64 бит.
    VarintOverflow,
    /// Стоят зарезервированные биты 5–7 байта присутствия.
    ReservedPresence(u8),
    /// Значение канала после дельты вне диапазона его типа.
    OutOfRange,
}

/// Итератор показаний одной датаграммы, без аллокаций. После первой ошибки
/// больше ничего не отдаёт.
pub struct Readings<'a> {
    rest:      &'a [u8],
    prev_time: EpochMillis,
    prev:      [i64; CHANNELS],
    failed:    bool,
}

/// Начать разбор открытого текста: читает `base_time`.
pub fn decode(plain: &[u8]) -> Result<Readings<'_>, CodecError> {
    let mut rest = plain;
    let base = take_varint(&mut rest)?;
    Ok(Readings {
        rest,
        prev_time: EpochMillis(base),
        prev: [0; CHANNELS],
        failed: false,
    })
}

fn take_varint(rest: &mut &[u8]) -> Result<u64, CodecError> {
    let mut v = 0u64;
    for i in 0..VARINT_U64_MAX {
        let (&byte, tail) = rest.split_first().ok_or(CodecError::Truncated)?;
        *rest = tail;
        let bits = u64::from(byte & 0x7F);
        // Десятый байт несёт только 64-й бит.
        if i == VARINT_U64_MAX - 1 && bits > 1 {
            return Err(CodecError::VarintOverflow);
        }
        v |= bits << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(CodecError::VarintOverflow)
}

impl Readings<'_> {
    fn next_reading(&mut self) -> Result<Reading, CodecError> {
        let (&presence, tail) = self.rest.split_first().ok_or(CodecError::Truncated)?;
        self.rest = tail;
        if presence & RESERVED_PRESENCE != 0 {
            return Err(CodecError::ReservedPresence(presence));
        }
        let dt = unzigzag(take_varint(&mut self.rest)?);
        let time = EpochMillis((self.prev_time.0 as i64).wrapping_add(dt) as u64);
        self.prev_time = time;

        let mut channels = [None; CHANNELS];
        for (bit, (prev, out)) in self.prev.iter_mut().zip(channels.iter_mut()).enumerate() {
            if presence & (1 << bit) != 0 {
                let delta = unzigzag(take_varint(&mut self.rest)?);
                let v = prev.checked_add(delta).ok_or(CodecError::OutOfRange)?;
                *prev = v;
                *out = Some(v);
            }
        }
        Reading::from_channels(time, channels)
    }
}

impl Iterator for Readings<'_> {
    type Item = Result<Reading, CodecError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let item = self.next_reading();
        self.failed = item.is_err();
        Some(item)
    }
}
