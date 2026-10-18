//! Кодек показаний протокола v2: фиксированная точка + дельты + varint.
//!
//! Открытый текст датаграммы: `base_time` (varint), затем показания до конца
//! (счётчика нет). Показание: байт присутствия (биты 0–4 — pressure,
//! `baro_temp`, `co2`, `humidity`, `scd_temp`; биты 5–7 = 0), `dt` мс от предыдущего
//! показания (первое — от `base_time`; zigzag varint, время после
//! пересинхронизации NTP может идти назад), затем для каждого присутствующего
//! канала — zigzag varint дельта от предыдущего присутствующего значения
//! ЭТОГО канала.
//!
//! Состояние дельт живёт только внутри одной датаграммы (старт — нули ⇒ первое
//! значение абсолютное): потеря датаграммы не портит следующие.
//!
//! Расширенная запись: первый байт с битом 7 (у байта присутствия он
//! зарезервирован) = `0x80 | вид`. Вид 0 — снимок журнала здоровья:
//! `layout` (varint), `n` (varint), `n` слов (varint). Времени и дельт у неё
//! нет, цепочку показаний она не трогает.

use crate::health::{
    SNAPSHOT_WORDS,
    Snapshot,
};
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
        .then_some({
            // Диапазон проверен выше, поэтому усечения здесь быть не может.
            #[allow(clippy::cast_possible_truncation)]
            {
                v as i32
            }
        })
}

impl CentiPascal {
    const PER_PASCAL: f64 = 100.0;

    #[must_use]
    pub fn from_pascal(pa: f32) -> Option<Self> {
        scaled(pa, Self::PER_PASCAL).map(Self)
    }

    #[must_use]
    pub fn pascal(self) -> f64 {
        f64::from(self.0) / Self::PER_PASCAL
    }
}

impl MilliCelsius {
    const PER_CELSIUS: f64 = 1000.0;

    #[must_use]
    pub fn from_celsius(c: f32) -> Option<Self> {
        scaled(c, Self::PER_CELSIUS).map(Self)
    }

    #[must_use]
    pub fn celsius(self) -> f64 {
        f64::from(self.0) / Self::PER_CELSIUS
    }
}

impl MilliPercentRh {
    const PER_PERCENT: f64 = 1000.0;

    #[must_use]
    pub fn from_percent(rh: f32) -> Option<Self> {
        scaled(rh, Self::PER_PERCENT).map(Self)
    }

    #[must_use]
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
/// Первый байт расширенной записи.
const EXTENDED: u8 = 0x80;
/// Вид расширенной записи: снимок журнала здоровья.
const KIND_HEALTH: u8 = 0;
/// Биты 5–6 байта присутствия (бит 7 — признак расширенной записи).
const RESERVED_PRESENCE: u8 = !((1 << CHANNELS) - 1) & !EXTENDED;

/// Больше слов в снимке декодер не примет (своя раскладка — `SNAPSHOT_WORDS`;
/// запас — под снимки прошивок с другой раскладкой).
pub const HEALTH_WORDS_MAX: usize = 96;

/// Снимок журнала здоровья, как он пришёл: раскладку проверяет получатель
/// ([`Snapshot::from_record`]) — снимок чужой раскладки всё равно можно
/// показать сырым.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthRecord {
    pub layout: u32,
    pub words:  heapless::Vec<u32, HEALTH_WORDS_MAX>,
}

/// Запись открытого текста датаграммы.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "records are yielded one at a time and only on the receiver side; boxing needs alloc in no_std"
)]
pub enum Record {
    Reading(Reading),
    Health(HealthRecord),
}

impl Reading {
    /// Есть ли хоть один канал. Пустое показание прошивка в очередь не кладёт.
    #[must_use]
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

    fn from_channels(time: EpochMillis, ch: [Option<i64>; CHANNELS]) -> Result<Self, CodecError> {
        fn narrow<T: TryFrom<i64>>(v: Option<i64>) -> Result<Option<T>, CodecError> {
            v.map(|v| T::try_from(v).map_err(|_| CodecError::OutOfRange))
                .transpose()
        }
        Ok(Self {
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
    (v.wrapping_shl(1) ^ (v >> 63)).cast_unsigned()
}

const fn unzigzag(v: u64) -> i64 {
    (v >> 1).cast_signed() ^ -((v & 1).cast_signed())
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

/// Худшая длина varint u32.
const VARINT_U32_MAX: usize = varint_len(u32::MAX as u64);
/// Худший снимок своей раскладки: байт вида, `layout`, `n`, слова.
pub const HEALTH_MAX: usize = 1 + 2 * VARINT_U32_MAX + SNAPSHOT_WORDS * VARINT_U32_MAX;

/// Сколько места нужно кодировщику, чтобы влезло хотя бы одно показание.
pub const MIN_PLAIN: usize = VARINT_U64_MAX + READING_MAX;

/// Датаграмма отправителя: снимок первым и хотя бы одно показание за ним.
pub const MIN_PLAIN_WITH_HEALTH: usize = MIN_PLAIN + HEALTH_MAX;

// Снимок и одно показание худшего размера влезают в любую датаграмму ⇒
// отправитель всегда продвигается, даже если в бэклоге только худшие показания.
const _: () = assert!(MIN_PLAIN_WITH_HEALTH <= crate::datagram::PLAIN_CAPACITY);

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
    ///
    /// # Panics
    ///
    /// Panics when `buf` is too small to encode at least one worst-case
    /// reading.
    #[must_use]
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
    #[must_use]
    pub const fn plain_len(&self) -> usize {
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
        let dt = r
            .time
            .0
            .cast_signed()
            .wrapping_sub(self.prev_time.0.cast_signed());
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

    /// Добавить снимок журнала здоровья. `false` — места нет. Отправитель
    /// кладёт его первым: тогда за ним всегда влезает хотя бы одно показание.
    pub fn push_health(&mut self, snapshot: &Snapshot) -> bool {
        if self.buf.len() - self.len < HEALTH_MAX {
            return false;
        }
        self.buf[self.len] = EXTENDED | KIND_HEALTH;
        self.len += 1;
        self.put_varint(u64::from(crate::health::LAYOUT));
        self.put_varint(SNAPSHOT_WORDS as u64);
        for word in snapshot.0 {
            self.put_varint(u64::from(word));
        }
        true
    }

    fn put_varint(&mut self, mut v: u64) {
        // Место проверено до записи (`new` / `push` под READING_MAX).
        while v >= 0x80 {
            self.buf[self.len] = u8::try_from(v & 0x7f).expect("varint byte is at most 7 bits") | 0x80;
            self.len += 1;
            v >>= 7;
        }
        self.buf[self.len] = u8::try_from(v & 0x7f).expect("varint byte is at most 7 bits");
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
    /// Расширенная запись неизвестного вида.
    UnknownRecord(u8),
    /// Снимок здоровья длиннее `HEALTH_WORDS_MAX` слов.
    HealthTooLong(u64),
}

/// Итератор записей одной датаграммы, без аллокаций. После первой ошибки
/// больше ничего не отдаёт.
pub struct Records<'a> {
    rest:      &'a [u8],
    prev_time: EpochMillis,
    prev:      [i64; CHANNELS],
    failed:    bool,
}

/// Начать разбор открытого текста: читает `base_time`.
///
/// # Errors
///
/// Возвращает `CodecError::Truncated`, если в тексте нет полного `base_time`.
pub fn decode(plain: &[u8]) -> Result<Records<'_>, CodecError> {
    let mut rest = plain;
    let base = take_varint(&mut rest)?;
    Ok(Records {
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

fn take_u32(rest: &mut &[u8]) -> Result<u32, CodecError> {
    u32::try_from(take_varint(rest)?).map_err(|_| CodecError::OutOfRange)
}

impl Records<'_> {
    fn next_record(&mut self) -> Result<Record, CodecError> {
        let (&first, tail) = self.rest.split_first().ok_or(CodecError::Truncated)?;
        self.rest = tail;
        if first & EXTENDED == 0 {
            return self.next_reading(first).map(Record::Reading);
        }
        if first != EXTENDED | KIND_HEALTH {
            return Err(CodecError::UnknownRecord(first));
        }
        let layout = take_u32(&mut self.rest)?;
        let n = take_varint(&mut self.rest)?;
        if n > HEALTH_WORDS_MAX as u64 {
            return Err(CodecError::HealthTooLong(n));
        }
        let mut words = heapless::Vec::new();
        for _ in 0..n {
            words
                .push(take_u32(&mut self.rest)?)
                .expect("n is checked against the capacity above");
        }
        Ok(Record::Health(HealthRecord { layout, words }))
    }

    fn next_reading(&mut self, presence: u8) -> Result<Reading, CodecError> {
        if presence & RESERVED_PRESENCE != 0 {
            return Err(CodecError::ReservedPresence(presence));
        }
        let dt = unzigzag(take_varint(&mut self.rest)?);
        let time = EpochMillis(self.prev_time.0.cast_signed().wrapping_add(dt).cast_unsigned());
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

impl Iterator for Records<'_> {
    type Item = Result<Record, CodecError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let item = self.next_record();
        self.failed = item.is_err();
        Some(item)
    }
}
