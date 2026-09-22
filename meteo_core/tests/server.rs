use core::time::Duration;

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use meteo_core::codec::{
    Encoder,
    Ppm,
    Reading,
};
use meteo_core::datagram::{
    BootId,
    Header,
    LiveSecs,
    MAX_DATAGRAM,
    Seq,
    ServerSalt,
    UpHeader,
    open_ack,
    plain_area,
    seal,
};
use meteo_core::recv_window::SeqStatus;
use meteo_core::server::{
    BOOT_IDLE_FORGET,
    LiveLease,
    Server,
};
use meteo_core::wire::EpochMillis;

fn cipher() -> Aes128Gcm {
    Aes128Gcm::new(&(*b"supersecretkey!1").into())
}

fn reading(t: u64) -> Reading {
    Reading {
        time:      EpochMillis(t),
        pressure:  None,
        baro_temp: None,
        co2:       Some(Ppm(400 + t as u16)),
        humidity:  None,
        scd_temp:  None,
    }
}

fn datagram(boot: u32, seq: u32, ack_req: bool) -> Vec<u8> {
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut enc = Encoder::new(plain_area(&mut buf), EpochMillis(u64::from(seq)));
    assert!(enc.push(&reading(u64::from(seq))));
    let plain_len = enc.plain_len();
    let header = Header::Up(UpHeader {
        boot: BootId(boot),
        seq: Seq(seq),
        ack_req,
    });
    let len = seal(&cipher(), header, &mut buf, plain_len);
    buf[..len].to_vec()
}

const T0: Duration = Duration::from_secs(1);

#[test]
fn nothing_to_ack_before_ingest() {
    let mut server = Server::new(cipher(), ServerSalt(1));
    let mut dg = datagram(5, 1, true);
    let inc = server.receive(&mut dg, T0).unwrap();
    assert!(!inc.duplicate);
    assert_eq!(inc.collect_readings().unwrap(), vec![reading(1)]);
    // Запись не удалась ⇒ ingested не зван ⇒ подтверждать нечего.
    assert!(server.ack(BootId(5), LiveSecs::OFF).is_none());

    // Повтор той же датаграммы — не дубль: в базе её нет.
    let mut dg = datagram(5, 1, true);
    assert!(!server.receive(&mut dg, T0).unwrap().duplicate);
}

#[test]
fn ack_reports_ingested_window_and_duplicates_are_detected() {
    let mut server = Server::new(cipher(), ServerSalt(9));
    for seq in [1, 3] {
        let mut dg = datagram(5, seq, false);
        let inc = server.receive(&mut dg, T0).unwrap();
        assert!(!inc.duplicate);
        server.ingested(BootId(5), Seq(seq), T0);
    }
    let mut dg = datagram(5, 3, true);
    assert!(server.receive(&mut dg, T0).unwrap().duplicate);

    let mut ack = server.ack(BootId(5), LiveSecs(42)).unwrap();
    let (header, ack) = open_ack(&cipher(), &mut ack).unwrap();
    assert_eq!(header.salt, ServerSalt(9));
    assert_eq!(ack.boot, BootId(5));
    assert_eq!(ack.live_for, LiveSecs(42));
    assert_eq!(ack.window.status(Seq(3)), SeqStatus::Seen);
    assert_eq!(ack.window.status(Seq(2)), SeqStatus::NotSeen);
    assert_eq!(ack.window.status(Seq(1)), SeqStatus::Seen);

    // Номера подтверждений растут: nonce не повторяется.
    let mut next = server.ack(BootId(5), LiveSecs::OFF).unwrap();
    assert!(open_ack(&cipher(), &mut next).unwrap().0.ack_seq > header.ack_seq);
}

#[test]
fn boots_have_separate_windows_and_idle_ones_are_forgotten() {
    let mut server = Server::new(cipher(), ServerSalt(1));
    let mut dg = datagram(5, 1, false);
    server.receive(&mut dg, T0).unwrap();
    server.ingested(BootId(5), Seq(1), T0);

    let mut other = datagram(6, 1, false);
    assert!(!server.receive(&mut other, T0).unwrap().duplicate);

    // Сутки тишины от загрузки 5 — окно забыто, дубль больше не опознать.
    let late = T0 + BOOT_IDLE_FORGET;
    let mut dg = datagram(6, 2, false);
    server.receive(&mut dg, late).unwrap();
    let mut dg = datagram(5, 1, false);
    assert!(!server.receive(&mut dg, late).unwrap().duplicate);
}

#[test]
fn garbage_is_rejected() {
    let mut server = Server::new(cipher(), ServerSalt(1));
    let mut dg = datagram(5, 1, false);
    dg[12] ^= 1;
    assert!(server.receive(&mut dg, T0).is_err());
}

#[test]
fn live_lease() {
    let lease_len = Duration::from_secs(300);
    let mut lease = LiveLease::new();
    assert_eq!(lease.remaining(T0), LiveSecs::OFF);
    lease.request(T0, lease_len);
    assert_eq!(lease.remaining(T0), LiveSecs(300));
    assert_eq!(
        lease.remaining(T0 + Duration::from_millis(100_500)),
        LiveSecs(199)
    );
    // Более ранний запрос срок не укорачивает.
    lease.request(T0 - Duration::from_secs(1), lease_len);
    assert_eq!(lease.remaining(T0), LiveSecs(300));
    assert_eq!(lease.remaining(T0 + lease_len), LiveSecs::OFF);
}
