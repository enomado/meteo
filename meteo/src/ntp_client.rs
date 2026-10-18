// These NTP futures use Embassy's !Send Stack on one executor; that is expected here.
#![allow(clippy::future_not_send)]

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
    Timer,
    with_timeout,
};
use esp_println::println;
use meteo_core::codec::EpochMillis;
use meteo_core::health::Word;
use meteo_core::ntp::{
    BootClock,
    SharedBootEpoch,
};
use meteo_core::ntp_kiss::KissMemory;
use meteo_core::supervisor::Uptime;
use sntpc::{
    Error,
    NtpContext,
    NtpResult,
    get_time,
};
use sntpc_net_embassy::UdpSocketWrapper;

use crate::watchdog::{
    health,
    uptime,
};

const NTP_SERVER: &str = "pool.ntp.org";
const NTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(7);
const NTP_RETRY_DELAY: Duration = Duration::from_secs(5);
const NTP_RESYNC_INTERVAL: Duration = Duration::from_secs(1000);

/// Epoch момента boot: читается из sensor-таски, пишется из ntp-таски. До
/// первого синка — `BootEpochMicros::BUILT_IN`; показания с таким временем в
/// серверный буфер НЕ попадают (`sensor_loop` держит их по uptime и ставит время
/// после `CLOCK_IS_SYNCED_WATCH`).
static BOOT_EPOCH: SharedBootEpoch = SharedBootEpoch::new();

/// Epoch момента `at` этой загрузки. Хранится epoch момента boot, поэтому момент
/// из прошлого (цикл до синка) пересчитывается так же, как текущий.
pub fn epoch_at(at: Uptime) -> EpochMillis {
    BOOT_EPOCH.load().epoch_at(at)
}

/// Локальные часы sntpc — `BOOT_EPOCH` + uptime, а не голый uptime: смещение
/// sntpc живёт в окне ±68 лет от них (см. `meteo_core::ntp`).
type NtpClock = BootClock<fn() -> Uptime>;

/// Почему попытка синка не дала времени.
enum NoTime {
    /// Запрос не ушёл или ответ отвергнут — сбой.
    Failed,
    /// DNS выдал сервер, которому после Kiss-o'-Death слать нельзя; запроса не
    /// было. Следующий DNS-запрос к пулу обычно даёт другой адрес.
    Shunned,
}

async fn ntp_sync(stack: Stack<'_>, clock: NtpClock, kisses: &mut KissMemory) -> Result<NtpResult, NoTime> {
    // Create UDP socket
    let mut rx_meta = [PacketMetadata::EMPTY; 16];
    let mut rx_buffer = [0; 4096];
    let mut tx_meta = [PacketMetadata::EMPTY; 16];
    let mut tx_buffer = [0; 4096];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    // Порт 0 ⇒ embassy-net выдаёт динамический. Исходящий порт 123 режут
    // некоторые провайдеры, а серверу он не нужен: ответ идёт на порт запроса.
    if let Err(error) = socket.bind(0) {
        println!("ntp: UDP bind failed: {:?}", error);
        return Err(NoTime::Failed);
    }

    let context = NtpContext::new(clock);

    let ntp_addrs = match stack.dns_query(NTP_SERVER, DnsQueryType::A).await {
        Ok(addrs) => addrs,
        Err(error) => {
            println!("ntp: DNS query for {} failed: {:?}", NTP_SERVER, error);
            return Err(NoTime::Failed);
        }
    };

    if ntp_addrs.is_empty() {
        println!("Failed to resolve DNS");
        return Err(NoTime::Failed);
    }

    let addr: IpAddr = ntp_addrs[0].into();
    if let Err(shunned) = kisses.admit(addr, uptime()) {
        println!("ntp: {} skipped after its kiss-o'-death: {:?}", addr, shunned);
        return Err(NoTime::Shunned);
    }
    let sock_addr = SocketAddr::from((addr, 123));

    let wrapped = UdpSocketWrapper::from(socket);

    println!("ntp: sending to {:?}", sock_addr);
    let result = get_time(sock_addr, &wrapped, context).await;

    match result {
        Ok(s) => {
            println!("ntp: ok, offset={}", s.offset());
            Ok(s)
        }
        // RFC 5905 §7.4: на DENY/RSTR сервер бросить, на RATE отступить.
        Err(Error::KissOfDeath(code)) => {
            let shunned = kisses.kissed(addr, code, uptime());
            println!(
                "ntp: kiss-o'-death {} from {}: {:?}",
                code.as_str(),
                addr,
                shunned
            );
            Err(NoTime::Failed)
        }
        Err(e) => {
            println!("ntp: error {:?}", e);
            Err(NoTime::Failed)
        }
    }
}

pub static CLOCK_IS_SYNCED_WATCH: Watch<CriticalSectionRawMutex, bool, 2> = Watch::new();
// MultiWakerRegistration

/// Одна попытка NTP-запроса с таймаутом. На успехе обновляет `BOOT_EPOCH`.
async fn try_sync(stack: Stack<'_>, kisses: &mut KissMemory) -> bool {
    println!("checking time");

    let clock: NtpClock = BootClock::new(BOOT_EPOCH.load(), uptime);
    let Ok(res) = with_timeout(NTP_REQUEST_TIMEOUT, ntp_sync(stack, clock, kisses)).await else {
        println!("ntp: timeout!");
        health().bump(Word::NtpFailures);
        return false;
    };

    let pp = match res {
        Ok(pp) => pp,
        Err(NoTime::Failed) => {
            health().bump(Word::NtpFailures);
            return false;
        }
        Err(NoTime::Shunned) => return false,
    };

    match clock.synced(&pp) {
        Ok(boot) => BOOT_EPOCH.store(boot),
        Err(rejected) => {
            println!(
                "ntp: offset {} us puts boot before the firmware build, ignored",
                rejected.claimed_offset_us
            );
            health().bump(Word::NtpFailures);
            return false;
        }
    }
    // Журнал ставит время сбоям по этому epoch (и восстановит время
    // зависания после reset).
    let now = uptime();
    health().clock_synced(now, epoch_at(now).0);
    true
}

#[embassy_executor::task]
pub async fn ntp_sync_loop(stack: Stack<'static>) {
    stack.wait_link_up().await;
    stack.wait_config_up().await;

    let flag_sender = CLOCK_IS_SYNCED_WATCH.sender();
    let mut kisses = KissMemory::new();

    // До первого sync sensor_loop держит показания по uptime и ставит им время
    // по флагу CLOCK_IS_SYNCED_WATCH; пока не получится — ретрай каждые
    // NTP_RETRY_DELAY (серверу после KoD — не чаще, чем велит `kisses`).
    while !try_sync(stack, &mut kisses).await {
        Timer::after(NTP_RETRY_DELAY).await;
    }
    flag_sender.send(true);

    // Подстройка — раз в NTP_RESYNC_INTERVAL, ошибки игнорим до следующей итерации.
    loop {
        Timer::after(NTP_RESYNC_INTERVAL).await;
        let _ = try_sync(stack, &mut kisses).await;
    }
}
