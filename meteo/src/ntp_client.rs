use smoltcp::wire::DnsQueryType;
use sntpc::{NtpContext, NtpResult, NtpTimestampGenerator, get_time};

use core::net::IpAddr;

use embassy_executor::Spawner;
use embassy_net::{
    Stack,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    watch::Watch,
};
use embassy_time::{Duration, Instant, Timer, with_timeout};

use esp_println::println;

const NTP_SERVER: &str = "pool.ntp.org";
pub static CURRENT_OFFSET: Mutex<CriticalSectionRawMutex, i64> = Mutex::new(1757986271840363);

#[derive(Copy, Clone)]
struct Timestamp {
    instant: Instant,
    offset: i64,
}

impl Timestamp {
    fn new(offset: i64) -> Self {
        Self {
            instant: Instant::now(),
            offset,
        }
    }
}

pub fn get_current_time_epoch() -> u64 {
    // оказалось что оффсет всётаки от epoch
    let instant = Instant::now();

    let ntp_offset = CURRENT_OFFSET.lock(|s| s.clone()) / 1000;

    let epoch_time = instant.as_millis() + ntp_offset as u64;

    epoch_time
}

impl NtpTimestampGenerator for Timestamp {
    fn init(&mut self) {
        self.instant = Instant::now();
    }

    fn timestamp_sec(&self) -> u64 {
        let total_micros = self.instant.as_micros() as i64 + self.offset as i64;
        let seconds = (total_micros / 1_000_000) as u64;
        seconds
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        let total_micros = self.instant.as_micros() as i64 + self.offset as i64;
        // Добавление 1_000_000 гарантирует корректное значение при отрицательном offset.
        // let subsec = total_micros % 1_000_000; // всегда 0..999_999
        let subsec = ((total_micros % 1_000_000) + 1_000_000) % 1_000_000;

        // println!("sub {:?}", subsec);
        subsec as u32
    }
}

pub async fn ntp_sync<'a>(stack: Stack<'a>) -> Option<NtpResult> {
    // Create UDP socket
    let mut rx_meta = [PacketMetadata::EMPTY; 16];
    let mut rx_buffer = [0; 4096];
    let mut tx_meta = [PacketMetadata::EMPTY; 16];
    let mut tx_buffer = [0; 4096];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(123).ok()?;

    let p = CURRENT_OFFSET.lock(|s| s.clone());

    // println!("current offset: {}", p);

    let context = NtpContext::new(Timestamp::new(p));

    let ntp_addrs = stack.dns_query(NTP_SERVER, DnsQueryType::A).await.ok()?;

    if ntp_addrs.is_empty() {
        println!("Failed to resolve DNS");
        return None;
    }

    use sntpc::net::SocketAddr;

    let addr: IpAddr = ntp_addrs[0].into();
    let sock_addr = SocketAddr::from((addr, 123));

    let result = get_time(sock_addr, &socket, context).await;

    match &result {
        Ok(s) => {}
        Err(e) => {
            println!("{:?}", e);
        }
    }

    result.ok()
}

pub static CLOCK_IS_SYNCED_WATCH: Watch<CriticalSectionRawMutex, bool, 2> = Watch::new();
// MultiWakerRegistration

#[embassy_executor::task]
pub async fn ntp_sync_loop(stack: Stack<'static>, spawner: Spawner) {
    //
    stack.wait_link_up().await;
    stack.wait_config_up().await;

    let flag_sender = CLOCK_IS_SYNCED_WATCH.sender();

    loop {
        println!("checking time");

        let p = with_timeout(Duration::from_secs(7), ntp_sync(stack))
            .await
            .ok()
            .and_then(|s| s);

        if let Some(pp) = p {
            let off = pp.offset() as i64;
            unsafe { CURRENT_OFFSET.lock_mut(|s| *s += off) };

            flag_sender.send(true);
        }

        Timer::after(Duration::from_millis(1000_000)).await;
    }
}
