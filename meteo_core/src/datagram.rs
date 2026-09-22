//! Датаграмма протокола v2 (UDP): заголовок, шифрование, подтверждение.
//!
//! On-wire: `[заголовок 9 Б][шифротекст][тег 16 Б]`, AES-128-GCM. Заголовок
//! открытый и целиком идёт в AAD ⇒ подмена любого его бита = отказ тега.
//!
//! - `b0`: биты 7..4 — версия (`VERSION`), бит 3 — направление (`0` прошивка →
//!   сервер, `1` сервер → прошивка), бит 0 — `ACK_REQ` (только прошивка →
//!   сервер). Прочие биты — `0`, иначе отказ разбора.
//! - прошивка → сервер: `BootId` (u32 BE) + `Seq` (u32 BE);
//! - сервер → прошивка: `ServerSalt` (u32 BE) + `AckSeq` (u32 BE).
//!
//! Nonce = `[направление, 0, 0, 0] ‖ id ‖ счётчик` — приёмник берёт его ИЗ
//! заголовка, а не считает сам ⇒ потеря, перестановка и дубль датаграмм
//! шифрованию безразличны. Уникальность nonce держит отправитель: `Seq` растёт
//! на каждую датаграмму (и на переотправку), `BootId`/`ServerSalt` случайны на
//! каждую загрузку/процесс.

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{
    AeadInOut,
    Nonce,
    Tag,
};

use crate::recv_window::RecvWindow;

/// Версия формата в старшей тетраде `b0`.
pub const VERSION: u8 = 1;
/// Наибольшая датаграмма (полезная нагрузка UDP). 1200 Б на уровне IP — без
/// IP-фрагментации: потеря одного фрагмента губит всю датаграмму.
pub const MAX_DATAGRAM: usize = 1172;
/// Заголовки IPv4 (20 Б) + UDP (8 Б): учёт трафика на уровне IP.
pub const IP_UDP_OVERHEAD: usize = 28;
pub const HEADER_LEN: usize = 9;
pub const TAG_LEN: usize = 16;
/// Сколько открытого текста влезает в одну датаграмму.
pub const PLAIN_CAPACITY: usize = MAX_DATAGRAM - HEADER_LEN - TAG_LEN;

/// Буфер одной датаграммы.
pub type DatagramBuf = [u8; MAX_DATAGRAM];

const VERSION_SHIFT: u32 = 4;
const DOWN_BIT: u8 = 1 << 3;
const ACK_REQ_BIT: u8 = 1 << 0;

/// Идентификатор загрузки прошивки: случайный u32 из аппаратного RNG на
/// каждом boot. Половина nonce датаграмм прошивки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BootId(pub u32);

/// Номер датаграммы прошивки внутри загрузки, с `FIRST`. Растёт на каждую
/// датаграмму, в том числе на переотправку тех же показаний.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u32);

impl Seq {
    pub const FIRST: Seq = Seq(1);

    pub fn next(self) -> Seq {
        // 2³² датаграмм — 136 лет при одной в секунду.
        Seq(self
            .0
            .checked_add(1)
            .expect("Seq overflow: 2^32 datagrams in one boot"))
    }
}

/// Случайная соль процесса приёмника: половина nonce его подтверждений.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerSalt(pub u32);

/// Номер подтверждения внутри процесса приёмника, с `FIRST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AckSeq(pub u32);

impl AckSeq {
    pub const FIRST: AckSeq = AckSeq(1);

    pub fn next(self) -> AckSeq {
        AckSeq(
            self.0
                .checked_add(1)
                .expect("AckSeq overflow: 2^32 acks in one process"),
        )
    }
}

/// Сколько секунд прошивке держать живой режим; `0` — живой режим не нужен.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveSecs(pub u16);

impl LiveSecs {
    pub const OFF: LiveSecs = LiveSecs(0);
}

/// Заголовок датаграммы прошивка → сервер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpHeader {
    pub boot:    BootId,
    pub seq:     Seq,
    /// Прошивка ждёт подтверждения на эту датаграмму.
    pub ack_req: bool,
}

/// Заголовок датаграммы сервер → прошивка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownHeader {
    pub salt:    ServerSalt,
    pub ack_seq: AckSeq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    Up(UpHeader),
    Down(DownHeader),
}

impl Header {
    fn b0(&self) -> u8 {
        let version = VERSION << VERSION_SHIFT;
        match self {
            Header::Up(h) => version | if h.ack_req { ACK_REQ_BIT } else { 0 },
            Header::Down(_) => version | DOWN_BIT,
        }
    }

    /// `(id, счётчик)` — вторая и третья части nonce.
    fn id_counter(&self) -> (u32, u32) {
        match self {
            Header::Up(h) => (h.boot.0, h.seq.0),
            Header::Down(h) => (h.salt.0, h.ack_seq.0),
        }
    }

    fn to_bytes(self) -> [u8; HEADER_LEN] {
        let (id, counter) = self.id_counter();
        let mut bytes = [0u8; HEADER_LEN];
        bytes[0] = self.b0();
        bytes[1..5].copy_from_slice(&id.to_be_bytes());
        bytes[5..9].copy_from_slice(&counter.to_be_bytes());
        bytes
    }

    fn nonce(&self) -> Nonce<Aes128Gcm> {
        let (id, counter) = self.id_counter();
        let mut bytes = [0u8; 12];
        bytes[0] = self.b0() & DOWN_BIT;
        bytes[4..8].copy_from_slice(&id.to_be_bytes());
        bytes[8..12].copy_from_slice(&counter.to_be_bytes());
        Nonce::<Aes128Gcm>::from(bytes)
    }
}

/// Почему датаграмма отвергнута.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramError {
    /// Короче заголовка с тегом или длиннее `MAX_DATAGRAM`.
    Length(usize),
    /// Чужая версия или ненулевые зарезервированные биты `b0`.
    BadHeader(u8),
    /// Датаграмма идёт не в ту сторону (например, эхо своей же датаграммы).
    WrongDirection,
    /// Тег не сошёлся: чужой ключ, подменённый заголовок или порча данных.
    Decrypt,
    /// Расшифровалось, но длина подтверждения не `ACK_LEN`.
    AckLength(usize),
}

/// Разбор заголовка без расшифровки (тег ещё не проверен — доверять
/// содержимому нельзя; нужен тестам и логам).
pub fn parse_header(datagram: &[u8]) -> Result<Header, DatagramError> {
    if !(HEADER_LEN + TAG_LEN..=MAX_DATAGRAM).contains(&datagram.len()) {
        return Err(DatagramError::Length(datagram.len()));
    }
    let b0 = datagram[0];
    let known_bits = (0xF << VERSION_SHIFT) | DOWN_BIT | ACK_REQ_BIT;
    if b0 >> VERSION_SHIFT != VERSION || b0 & !known_bits != 0 {
        return Err(DatagramError::BadHeader(b0));
    }
    let word = |at: usize| u32::from_be_bytes(datagram[at..at + 4].try_into().expect("4-byte slice"));
    let (id, counter) = (word(1), word(5));
    if b0 & DOWN_BIT == 0 {
        Ok(Header::Up(UpHeader {
            boot:    BootId(id),
            seq:     Seq(counter),
            ack_req: b0 & ACK_REQ_BIT != 0,
        }))
    } else if b0 & ACK_REQ_BIT != 0 {
        // ACK_REQ определён только для направления прошивка → сервер.
        Err(DatagramError::BadHeader(b0))
    } else {
        Ok(Header::Down(DownHeader {
            salt:    ServerSalt(id),
            ack_seq: AckSeq(counter),
        }))
    }
}

/// Открытый текст датаграммы в `buf` — сюда пишет кодировщик перед `seal`.
pub fn plain_area(buf: &mut DatagramBuf) -> &mut [u8] {
    &mut buf[HEADER_LEN..HEADER_LEN + PLAIN_CAPACITY]
}

/// Собирает датаграмму в `buf`: открытый текст уже лежит в
/// `buf[HEADER_LEN..HEADER_LEN + plain_len]` (см. [`plain_area`]). Пишет
/// заголовок, шифрует на месте, дописывает тег; возвращает длину датаграммы.
pub fn seal(cipher: &Aes128Gcm, header: Header, buf: &mut [u8], plain_len: usize) -> usize {
    let plain_end = HEADER_LEN + plain_len;
    let len = plain_end + TAG_LEN;
    assert!(
        len <= buf.len() && len <= MAX_DATAGRAM,
        "datagram of {len} bytes exceeds the buffer or MAX_DATAGRAM"
    );
    buf[..HEADER_LEN].copy_from_slice(&header.to_bytes());
    let (head, rest) = buf.split_at_mut(HEADER_LEN);
    let tag = cipher
        .encrypt_inout_detached(&header.nonce(), head, (&mut rest[..plain_len]).into())
        .expect("plaintext is under MAX_DATAGRAM, far below the AES-GCM length limit");
    buf[plain_end..len].copy_from_slice(&tag);
    len
}

/// Проверяет и расшифровывает датаграмму на месте; возвращает заголовок и
/// открытый текст (срез внутри `datagram`).
pub fn open<'a>(cipher: &Aes128Gcm, datagram: &'a mut [u8]) -> Result<(Header, &'a [u8]), DatagramError> {
    let header = parse_header(datagram)?;
    let tag_at = datagram.len() - TAG_LEN;
    let (body, tag_bytes) = datagram.split_at_mut(tag_at);
    let tag: Tag<Aes128Gcm> = (&*tag_bytes).try_into().expect("split exactly TAG_LEN bytes");
    let (head, ciphertext) = body.split_at_mut(HEADER_LEN);
    cipher
        .decrypt_inout_detached(&header.nonce(), head, ciphertext.into(), &tag)
        .map_err(|_| DatagramError::Decrypt)?;
    Ok((header, ciphertext))
}

/// [`open`] для приёмника: принимает только направление прошивка → сервер.
pub fn open_up<'a>(
    cipher: &Aes128Gcm,
    datagram: &'a mut [u8],
) -> Result<(UpHeader, &'a [u8]), DatagramError> {
    match open(cipher, datagram)? {
        (Header::Up(h), plain) => Ok((h, plain)),
        (Header::Down(_), _) => Err(DatagramError::WrongDirection),
    }
}

/// [`open`] для прошивки: только подтверждение сервера.
pub fn open_ack(cipher: &Aes128Gcm, datagram: &mut [u8]) -> Result<(DownHeader, Ack), DatagramError> {
    match open(cipher, datagram)? {
        (Header::Down(h), plain) => Ok((h, Ack::from_bytes(plain)?)),
        (Header::Up(_), _) => Err(DatagramError::WrongDirection),
    }
}

/// Длина зашифрованной части подтверждения.
pub const ACK_LEN: usize = 14;
/// Датаграмма подтверждения целиком.
pub const ACK_DATAGRAM_LEN: usize = HEADER_LEN + ACK_LEN + TAG_LEN;

/// Подтверждение сервера: какие датаграммы загрузки `boot` записаны в базу
/// (окно приёма) и нужен ли живой режим.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub boot:     BootId,
    pub window:   RecvWindow,
    pub live_for: LiveSecs,
}

impl Ack {
    /// `boot ‖ highest ‖ seen ‖ live_for`, всё BE.
    pub fn to_bytes(&self) -> [u8; ACK_LEN] {
        let mut bytes = [0u8; ACK_LEN];
        bytes[0..4].copy_from_slice(&self.boot.0.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.window.highest().0.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.window.seen_bits().to_be_bytes());
        bytes[12..14].copy_from_slice(&self.live_for.0.to_be_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Ack, DatagramError> {
        let bytes: &[u8; ACK_LEN] = bytes
            .try_into()
            .map_err(|_| DatagramError::AckLength(bytes.len()))?;
        let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4-byte slice"));
        Ok(Ack {
            boot:     BootId(word(0)),
            window:   RecvWindow::from_parts(Seq(word(4)), word(8)),
            live_for: LiveSecs(u16::from_be_bytes([bytes[12], bytes[13]])),
        })
    }

    /// Готовая датаграмма подтверждения.
    pub fn seal(&self, cipher: &Aes128Gcm, header: DownHeader) -> [u8; ACK_DATAGRAM_LEN] {
        let mut buf = [0u8; ACK_DATAGRAM_LEN];
        buf[HEADER_LEN..HEADER_LEN + ACK_LEN].copy_from_slice(&self.to_bytes());
        let len = seal(cipher, Header::Down(header), &mut buf, ACK_LEN);
        debug_assert_eq!(len, ACK_DATAGRAM_LEN);
        buf
    }
}
