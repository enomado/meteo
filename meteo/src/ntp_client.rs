use core::net::{
    IpAddr,
    SocketAddr,
};

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{
    PacketMetadata,
    UdpSocket,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::watch::Watch;
use embassy_time::{
    Duration,
    Instant,
    Timer,
    with_timeout,
};
use esp_println::println;
use meteo_core::wire::EpochMillis;
use portable_atomic::{
    AtomicI64,
    Ordering,
};
use sntpc::{
    NtpContext,
    NtpResult,
    get_time,
};
use sntpc_net_embassy::UdpSocketWrapper;
use sntpc_time_embassy::EmbassyTimestampGenerator;

const NTP_SERVER: &str = "pool.ntp.org";
const NTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(7);
const NTP_RETRY_DELAY: Duration = Duration::from_secs(5);
const NTP_RESYNC_INTERVAL: Duration = Duration::from_secs(1000);

/// Смещение от локальных часов к wall-clock, в МИКРОсекундах: так его отдаёт
/// `NtpResult::offset()`, а `EmbassyTimestampGenerator` считает «системным
/// временем» `Instant` с момента boot ⇒ offset == wall-clock на буте.
/// Атомик (не `Mutex` + `unsafe lock_mut`): значение скалярное, читается из
/// sensor-таски, пишется из ntp-таски.
static CURRENT_OFFSET_US: AtomicI64 = AtomicI64::new(DEFAULT_OFFSET_US);

/// Baked-in значение до первого успешного NTP-синка: 2025-09-16 01:31 UTC.
/// Показания с таким временем в серверный буфер НЕ попадают (sensor_loop ждёт
/// `CLOCK_IS_SYNCED_WATCH`), оно нужно лишь чтобы часы вообще были монотонны.
const DEFAULT_OFFSET_US: i64 = 1_757_986_271_840_363;

pub fn now_epoch() -> EpochMillis {
    let since_boot_ms = Instant::now().as_millis();
    let offset_ms = CURRENT_OFFSET_US.load(Ordering::Relaxed) / 1000;
    // wrapping: при вменяемом оффсете переполнения нет, но паника здесь морозила
    // бы чип (см. crate::watchdog) — цена битого таймстампа несопоставима.
    EpochMillis(since_boot_ms.wrapping_add(offset_ms as u64))
}

async fn ntp_sync<'a>(stack: Stack<'a>) -> Option<NtpResult> {
    // Create UDP socket
    let mut rx_meta = [PacketMetadata::EMPTY; 16];
    let mut rx_buffer = [0; 4096];
    let mut tx_meta = [PacketMetadata::EMPTY; 16];
    let mut tx_buffer = [0; 4096];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    // Порт 0 ⇒ embassy-net выдаёт динамический. Исходящий порт 123 режут
    // некоторые провайдеры, а серверу он не нужен: ответ идёт на порт запроса.
    socket.bind(0).ok()?;

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

/// Одна попытка NTP-запроса с таймаутом. На успехе обновляет `CURRENT_OFFSET_US`.
async fn try_sync(stack: Stack<'_>) -> bool {
    println!("checking time");

    let res = match with_timeout(NTP_REQUEST_TIMEOUT, ntp_sync(stack)).await {
        Ok(inner) => inner,
        Err(_) => {
            println!("ntp: timeout!");
            return false;
        }
    };

    let Some(pp) = res else { return false };

    CURRENT_OFFSET_US.store(pp.offset(), Ordering::Relaxed);
    true
}

#[embassy_executor::task]
pub async fn ntp_sync_loop(stack: Stack<'static>) {
    stack.wait_link_up().await;
    stack.wait_config_up().await;

    let flag_sender = CLOCK_IS_SYNCED_WATCH.sender();

    // Первый sync блокирует консьюмеров (sensor_loop ждёт CLOCK_IS_SYNCED_WATCH);
    // пока не получится — ретрай каждые NTP_RETRY_DELAY.
    while !try_sync(stack).await {
        Timer::after(NTP_RETRY_DELAY).await;
    }
    flag_sender.send(true);

    // Подстройка — раз в NTP_RESYNC_INTERVAL, ошибки игнорим до следующей итерации.
    loop {
        Timer::after(NTP_RESYNC_INTERVAL).await;
        let _ = try_sync(stack).await;
    }
}
