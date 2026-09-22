use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use meteo_core::datagram::{
    ACK_DATAGRAM_LEN,
    Ack,
    AckSeq,
    BootId,
    DatagramError,
    DownHeader,
    HEADER_LEN,
    Header,
    LiveSecs,
    MAX_DATAGRAM,
    PLAIN_CAPACITY,
    Seq,
    ServerSalt,
    TAG_LEN,
    UpHeader,
    open,
    open_ack,
    open_up,
    parse_header,
    plain_area,
    seal,
};
use meteo_core::recv_window::RecvWindow;

fn cipher() -> Aes128Gcm {
    Aes128Gcm::new(&(*b"supersecretkey!1").into())
}

const UP: UpHeader = UpHeader {
    boot:    BootId(0xDEAD_BEEF),
    seq:     Seq(7),
    ack_req: true,
};

/// Датаграмма прошивки с открытым текстом `plain`.
fn up_datagram(header: UpHeader, plain: &[u8]) -> Vec<u8> {
    let mut buf = [0u8; MAX_DATAGRAM];
    plain_area(&mut buf)[..plain.len()].copy_from_slice(plain);
    let len = seal(&cipher(), Header::Up(header), &mut buf, plain.len());
    buf[..len].to_vec()
}

#[test]
fn up_roundtrip_and_header_layout() {
    let plain = b"readings";
    let mut dg = up_datagram(UP, plain);
    assert_eq!(dg.len(), HEADER_LEN + plain.len() + TAG_LEN);
    assert_eq!(&dg[..HEADER_LEN], &[0x11, 0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 7]);
    let (header, opened) = open_up(&cipher(), &mut dg).unwrap();
    assert_eq!(header, UP);
    assert_eq!(opened, plain);
}

#[test]
fn full_capacity_datagram_roundtrips() {
    let plain = vec![0xA5; PLAIN_CAPACITY];
    let mut dg = up_datagram(UP, &plain);
    assert_eq!(dg.len(), MAX_DATAGRAM);
    assert_eq!(open_up(&cipher(), &mut dg).unwrap().1, &plain[..]);
}

#[test]
fn any_flipped_bit_is_rejected() {
    let clean = up_datagram(UP, b"some readings");
    for byte in 0..clean.len() {
        for bit in 0..8 {
            let mut dg = clean.clone();
            dg[byte] ^= 1 << bit;
            let res = open(&cipher(), &mut dg);
            assert!(
                matches!(res, Err(DatagramError::Decrypt | DatagramError::BadHeader(_))),
                "byte {byte} bit {bit}: {res:?}"
            );
        }
    }
}

#[test]
fn header_is_authenticated_not_just_parsed() {
    // Смена номера в заголовке даёт валидный заголовок — отвергнуть обязан тег.
    let mut dg = up_datagram(UP, b"x");
    dg[8] = 8;
    assert_eq!(open(&cipher(), &mut dg).unwrap_err(), DatagramError::Decrypt);
}

#[test]
fn directions_do_not_share_nonces() {
    // Одинаковые (id, счётчик) в разных направлениях — разные nonce: шифротекст
    // одного и того же текста различается.
    let plain = [0u8; 14];
    let up = up_datagram(
        UpHeader {
            boot:    BootId(5),
            seq:     Seq(9),
            ack_req: false,
        },
        &plain,
    );
    let mut buf = [0u8; MAX_DATAGRAM];
    let len = seal(
        &cipher(),
        Header::Down(DownHeader {
            salt:    ServerSalt(5),
            ack_seq: AckSeq(9),
        }),
        &mut buf,
        plain.len(),
    );
    assert_eq!(len, up.len());
    assert_ne!(&buf[HEADER_LEN..len], &up[HEADER_LEN..]);
}

#[test]
fn wrong_direction_is_rejected() {
    let mut up = up_datagram(UP, &[0u8; 14]);
    assert_eq!(
        open_ack(&cipher(), &mut up).unwrap_err(),
        DatagramError::WrongDirection
    );

    let ack = Ack {
        boot:     BootId(1),
        window:   RecvWindow::new(Seq(3)),
        live_for: LiveSecs::OFF,
    };
    let mut down = ack
        .seal(
            &cipher(),
            DownHeader {
                salt:    ServerSalt(1),
                ack_seq: AckSeq::FIRST,
            },
        )
        .to_vec();
    assert_eq!(
        open_up(&cipher(), &mut down).unwrap_err(),
        DatagramError::WrongDirection
    );
}

#[test]
fn reserved_and_foreign_header_bits_are_rejected() {
    let clean = up_datagram(UP, b"x");
    for b0 in [0x00, 0x21, 0x13, 0x15, 0x19, 0x31, 0xF1] {
        let mut dg = clean.clone();
        dg[0] = b0;
        assert_eq!(
            parse_header(&dg),
            Err(DatagramError::BadHeader(b0)),
            "b0 {b0:#04x}"
        );
    }
    // ACK_REQ определён только для прошивки.
    let mut dg = clean.clone();
    dg[0] = 0x19;
    assert_eq!(parse_header(&dg), Err(DatagramError::BadHeader(0x19)));
}

#[test]
fn length_bounds() {
    for len in [0, 1, HEADER_LEN + TAG_LEN - 1, MAX_DATAGRAM + 1] {
        let mut dg = vec![0x10; len];
        assert_eq!(open(&cipher(), &mut dg), Err(DatagramError::Length(len)));
    }
    // Пустой открытый текст — допустимая длина (отвергнет уже тег).
    let mut dg = vec![0x10; HEADER_LEN + TAG_LEN];
    assert_eq!(open(&cipher(), &mut dg), Err(DatagramError::Decrypt));
}

#[test]
fn ack_roundtrip() {
    let mut window = RecvWindow::new(Seq(40));
    window.mark(Seq(38));
    window.mark(Seq(9));
    let ack = Ack {
        boot: BootId(0x0102_0304),
        window,
        live_for: LiveSecs(299),
    };
    let header = DownHeader {
        salt:    ServerSalt(77),
        ack_seq: AckSeq(12),
    };
    let mut dg = ack.seal(&cipher(), header);
    assert_eq!(dg.len(), ACK_DATAGRAM_LEN);
    assert_eq!(open_ack(&cipher(), &mut dg), Ok((header, ack)));
}

#[test]
fn ack_of_wrong_length_is_rejected() {
    let mut buf = [0u8; MAX_DATAGRAM];
    let len = seal(
        &cipher(),
        Header::Down(DownHeader {
            salt:    ServerSalt(1),
            ack_seq: AckSeq(1),
        }),
        &mut buf,
        13,
    );
    assert_eq!(
        open_ack(&cipher(), &mut buf[..len]),
        Err(DatagramError::AckLength(13))
    );
}
