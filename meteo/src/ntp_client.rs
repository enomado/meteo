use embassy_net::dns::DnsQueryType;
use sntpc::{NtpContext, NtpResult, get_time};
use sntpc_net_embassy::UdpSocketWrapper;
use sntpc_time_embassy::EmbassyTimestampGenerator;

use core::net::{IpAddr, SocketAddr};

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
// CURRENT_OFFSET = wall-clock-at-boot в микросекундах от UNIX epoch.
// epoch_now = Instant::now() (с момента boot) + CURRENT_OFFSET.
// Initial value — baked-in default до первого успешного NTP-sync.
pub static CURRENT_OFFSET: Mutex<CriticalSectionRawMutex, i64> = Mutex::new(1757986271840363);

pub fn get_current_time_epoch() -> u64 {
    let instant = Instant::now();
    let ntp_offset = CURRENT_OFFSET.lock(|s| s.clone()) / 1000;
    instant.as_millis() + ntp_offset as u64
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

    let context = NtpContext::new(EmbassyTimestampGenerator::default());

    let ntp_addrs = stack.dns_query(NTP_SERVER, DnsQueryType::A).await.ok()?;

    if ntp_addrs.is_empty() {
        println!("Failed to resolve DNS");
        return None;
    }

    let addr: IpAddr = ntp_addrs[0].into();
    let sock_addr = SocketAddr::from((addr, 123));

    let wrapped = UdpSocketWrapper::from(socket);

    println!("ntp: sending to {:?}", sock_addr);
    let result = get_time(sock_addr, &wrapped, context).await;

    match &result {
        Ok(s) => {
            println!("ntp: ok, offset={}", s.offset());
        }
        Err(e) => {
            println!("ntp: error {:?}", e);
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

        let p = with_timeout(Duration::from_secs(7), ntp_sync(stack)).await;

        if p.is_err() {
            println!("ntp: timeout!");
        }

        let p = p.ok().and_then(|s| s);

        if let Some(pp) = p {
            let off = pp.offset() as i64;
            unsafe { CURRENT_OFFSET.lock_mut(|s| *s = off) };

            flag_sender.send(true);
            break;
        }

        Timer::after(Duration::from_secs(5)).await;
    }

    // после синхронизации — периодическая подстройка
    loop {
        Timer::after(Duration::from_millis(1000_000)).await;

        println!("checking time");

        let p = with_timeout(Duration::from_secs(7), ntp_sync(stack)).await;

        if p.is_err() {
            println!("ntp: timeout!");
        }

        if let Some(pp) = p.ok().and_then(|s| s) {
            let off = pp.offset() as i64;
            unsafe { CURRENT_OFFSET.lock_mut(|s| *s = off) };
        }
    }
}
