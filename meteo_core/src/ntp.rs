//! Wall-clock по NTP: epoch момента загрузки и локальные часы для sntpc.
//!
//! sntpc считает смещение разностью 32.32-меток NTP с переполнением, т.е. только
//! в окне ±2^31 с (±68 лет) от локальных часов, и по ним же достраивает эру
//! метки сервера. Локальные часы «uptime как Unix-время» (так делает
//! `sntpc_time_embassy::EmbassyTimestampGenerator`) стоят в 1970 г.: смещение уже
//! ≈ 56 лет, а после 2038-01-19 оно молча переполняется. Поэтому локальные часы
//! здесь — последний известный epoch загрузки + uptime: смещение становится
//! поправкой к уже известному времени, окно едет с каждым синком. Первый синк
//! после загрузки опирается на вшитое время ⇒ работает до ~2093 г.

use portable_atomic::{
    AtomicU64,
    Ordering,
};
use sntpc::{
    NtpResult,
    NtpTimestampGenerator,
};

use crate::supervisor::Uptime;
use crate::wire::EpochMillis;

const MICROS_PER_SEC: u64 = 1_000_000;

/// Wall-clock момента boot этой загрузки, мкс от UNIX epoch.
///
/// Инвариант: не раньше [`Self::BUILT_IN`] — устройство не загружается раньше,
/// чем собрана прошивка. Синк, нарушающий его, отвергается
/// ([`BootClock::synced`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BootEpochMicros(u64);

impl BootEpochMicros {
    /// Вшитое значение до первого синка: 2025-09-16 01:31 UTC. Показания с
    /// таким временем на сервер не уходят (их держат по uptime до синка), оно
    /// нужно, чтобы часы были монотонны, и как опора первого синка.
    pub const BUILT_IN: Self = Self(1_757_986_271_840_363);

    /// Wall-clock момента `at` этой загрузки.
    #[must_use]
    pub fn epoch_at(self, at: Uptime) -> EpochMillis {
        EpochMillis(self.0.saturating_add(uptime_micros(at)) / 1000)
    }

    /// Микросекунды от UNIX epoch — для логов.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }
}

/// Синк поставил загрузку раньше [`BootEpochMicros::BUILT_IN`]: ответ сервера
/// не может быть верным.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootBeforeBuild {
    pub claimed_offset_us: i64,
}

/// Epoch загрузки, общий для NTP-таски (пишет) и остальных (читают).
pub struct SharedBootEpoch(AtomicU64);

impl SharedBootEpoch {
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(BootEpochMicros::BUILT_IN.0))
    }

    pub fn load(&self) -> BootEpochMicros {
        BootEpochMicros(self.0.load(Ordering::Relaxed))
    }

    pub fn store(&self, epoch: BootEpochMicros) {
        self.0.store(epoch.0, Ordering::Relaxed);
    }
}

impl Default for SharedBootEpoch {
    fn default() -> Self {
        Self::new()
    }
}

/// Локальные часы для sntpc: `boot` + uptime. Смещение из ответа измерено
/// относительно `boot` — применять его только через [`Self::synced`] того же
/// генератора.
#[derive(Clone, Copy)]
pub struct BootClock<C> {
    boot:   BootEpochMicros,
    uptime: C,
    /// Момент последнего `init`, мкс от UNIX epoch.
    now_us: u64,
}

impl<C: Fn() -> Uptime> BootClock<C> {
    pub const fn new(boot: BootEpochMicros, uptime: C) -> Self {
        Self {
            boot,
            uptime,
            now_us: boot.0,
        }
    }

    /// Epoch загрузки по ответу, полученному с этими часами.
    ///
    /// # Errors
    ///
    /// [`BootBeforeBuild`], если ответ ставит загрузку раньше сборки прошивки.
    pub fn synced(&self, result: &NtpResult) -> Result<BootEpochMicros, BootBeforeBuild> {
        let offset = result.offset();
        self.boot
            .0
            .checked_add_signed(offset)
            .map(BootEpochMicros)
            .filter(|boot| *boot >= BootEpochMicros::BUILT_IN)
            .ok_or(BootBeforeBuild {
                claimed_offset_us: offset,
            })
    }
}

impl<C: Fn() -> Uptime> NtpTimestampGenerator for BootClock<C> {
    fn init(&mut self) {
        self.now_us = self.boot.0.saturating_add(uptime_micros((self.uptime)()));
    }

    fn timestamp_sec(&self) -> u64 {
        self.now_us / MICROS_PER_SEC
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        u32::try_from(self.now_us % MICROS_PER_SEC).expect("remainder of division by 10^6 fits u32")
    }
}

fn uptime_micros(at: Uptime) -> u64 {
    u64::try_from(at.0.as_micros()).unwrap_or(u64::MAX)
}
