//! Формат пакета прошивка → приёмник. Единственное определение: прошивка
//! кодирует, `example_server` декодирует этим же кодом (раньше структуры были
//! скопированы в обе стороны и сверялись глазами — postcard кодирует поля по
//! порядку объявления, без имён).
//!
//! On-wire: `[u32 BE payload_len][AES-128-GCM ciphertext][16-byte tag]`,
//! `payload_len` = ciphertext_len + 16. Пустые associated data.
//! Nonce (96 бит): 4 нулевых байта + 8 байт BE-счётчика пакетов соединения,
//! первый пакет — 1.

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{
    AeadInOut,
    Nonce,
    Tag,
};
use postcard::experimental::max_size::MaxSize;
use serde::{
    Deserialize,
    Serialize,
};

/// Wall-clock в миллисекундах от UNIX epoch. Единица измерения — часть типа:
/// рядом ходят микросекунды NTP-оффсета и `Instant` с момента boot, и голый
/// `u64` их не различал. На проводе остаётся числом: postcard сериализует
/// newtype прозрачно.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, MaxSize)]
pub struct EpochMillis(pub u64);

/// BMP390 одно показание — pressure (Pa) + temperature (°C). Поля всегда
/// заполнены или отсутствуют синхронно (читаются одним вызовом).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, MaxSize)]
pub struct BaroReading {
    pub pressure: f32,
    pub temp:     f32,
}

/// SCD41 одно показание — CO2 (ppm), humidity (%), temperature (°C).
/// Поля всегда заполнены или отсутствуют синхронно (один read_measurement).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, MaxSize)]
pub struct ScdReading {
    pub co2:      u16,
    pub humidity: f32,
    pub temp:     f32,
}

/// Одна строка измерений. `baro`/`scd` независимы (датчик может отсутствовать
/// или не отдать данные в этом цикле). `MaxSize` держит бюджет пакета: новое
/// поле увеличит худший размер, и сборка упадёт, если он перестанет влезать.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, MaxSize)]
pub struct SensorData {
    pub baro: Option<BaroReading>,
    pub scd:  Option<ScdReading>,
    pub time: EpochMillis,
}

/// Длина префикса `u32 BE payload_len` перед шифротекстом.
pub const LEN_PREFIX: usize = 4;
/// AES-GCM tag, дописывается сразу за шифротекстом.
pub const TAG_LEN: usize = 16;
/// Пакет целиком: префикс + шифротекст + tag.
pub const BUF_LEN: usize = 1024;
/// Наибольший `payload_len`, который может прийти от прошивки.
pub const MAX_PAYLOAD_LEN: usize = BUF_LEN - LEN_PREFIX;

/// Максимум записей в один пакет.
pub const MAX_BATCH: usize = 24;

// Худший батч влезает в пакет: postcard пишет последовательность как
// varint-длину (usize) и элементы подряд. Компилятор проверяет то, что раньше
// держала арифметика в комментарии ⇒ `encode_packet` не может переполнить
// буфер (было: паника через .unwrap() → заморозка чипа, инцидент 2026-07-04).
const _: () = assert!(
    usize::POSTCARD_MAX_SIZE + MAX_BATCH * SensorData::POSTCARD_MAX_SIZE <= MAX_PAYLOAD_LEN - TAG_LEN
);

/// Содержимое одного пакета. Ёмкость = `MAX_BATCH`: бюджет пакета доказан
/// для неё, поэтому больший батч не собрать и типом.
pub type SensorBatch = heapless::Vec<SensorData, MAX_BATCH>;

/// Буфер одного пакета на проводе.
pub type PacketBuf = [u8; BUF_LEN];

/// Номер пакета внутри TCP-соединения; из него строится nonce. Обе стороны
/// считают с `FIRST` заново на каждом соединении.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketCounter(pub u64);

impl PacketCounter {
    pub const FIRST: PacketCounter = PacketCounter(1);

    pub fn next(self) -> PacketCounter {
        PacketCounter(self.0 + 1)
    }

    /// 4 нулевых байта + 8 байт BE-счётчика. ВНИМАНИЕ: счётчик обнуляется на
    /// каждом соединении ⇒ пара (ключ, nonce) повторяется на разных данных.
    /// Известная дыра; протокол v2 (`datagram`) её закрывает: nonce из
    /// `BootId` и `Seq`, растущего на каждую датаграмму.
    fn nonce(self) -> Nonce<Aes128Gcm> {
        let mut bytes = [0u8; 12];
        bytes[4..].copy_from_slice(&self.0.to_be_bytes());
        Nonce::<Aes128Gcm>::from(bytes)
    }
}

/// Кодирует батч в пакет в `buf` (шифрование in-place, без heap), возвращает
/// длину пакета — `&buf[..len]` уходит в сокет целиком.
pub fn encode_packet(
    cipher: &Aes128Gcm,
    counter: PacketCounter,
    batch: &SensorBatch,
    buf: &mut PacketBuf,
) -> usize {
    let body = &mut buf[LEN_PREFIX..BUF_LEN - TAG_LEN];
    let plain_len = postcard::to_slice(batch.as_slice(), body)
        .expect("packet budget checked at compile time")
        .len();

    let plain_end = LEN_PREFIX + plain_len;
    let tag = cipher
        .encrypt_inout_detached(&counter.nonce(), b"", (&mut buf[LEN_PREFIX..plain_end]).into())
        .expect("plaintext is under BUF_LEN, far below the AES-GCM length limit");
    buf[plain_end..plain_end + TAG_LEN].copy_from_slice(&tag);

    let payload_len = plain_len + TAG_LEN;
    let payload_len_u32 = u32::try_from(payload_len).expect("payload is under BUF_LEN");
    buf[..LEN_PREFIX].copy_from_slice(&payload_len_u32.to_be_bytes());

    LEN_PREFIX + payload_len
}

/// Почему приёмник отверг пакет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketError {
    /// `payload_len` вне `TAG_LEN..=MAX_PAYLOAD_LEN`: такого прошивка не шлёт,
    /// поток рассинхронизирован или это не наш клиент.
    LengthOutOfRange(usize),
    /// Tag не сошёлся: чужой ключ, сбитый счётчик или порча данных.
    Decrypt,
    /// Расшифровалось, но не разбирается как батч.
    Decode(postcard::Error),
}

/// Длина payload по префиксу. Проверяется ДО чтения payload, чтобы битый
/// префикс не заставил приёмник читать/аллоцировать мегабайты.
pub fn payload_len(prefix: [u8; LEN_PREFIX]) -> Result<usize, PacketError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if (TAG_LEN..=MAX_PAYLOAD_LEN).contains(&len) {
        Ok(len)
    } else {
        Err(PacketError::LengthOutOfRange(len))
    }
}

/// Расшифровывает payload (шифротекст + tag) на месте и разбирает батч.
pub fn decode_payload(
    cipher: &Aes128Gcm,
    counter: PacketCounter,
    payload: &mut [u8],
) -> Result<SensorBatch, PacketError> {
    if !(TAG_LEN..=MAX_PAYLOAD_LEN).contains(&payload.len()) {
        return Err(PacketError::LengthOutOfRange(payload.len()));
    }
    let (ciphertext, tag_bytes) = payload.split_at_mut(payload.len() - TAG_LEN);
    let tag: Tag<Aes128Gcm> = (&*tag_bytes).try_into().expect("split exactly TAG_LEN bytes");
    cipher
        .decrypt_inout_detached(&counter.nonce(), b"", ciphertext.into(), &tag)
        .map_err(|_| PacketError::Decrypt)?;
    postcard::from_bytes(ciphertext).map_err(PacketError::Decode)
}
