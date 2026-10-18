//! Реакция NTP-клиента на Kiss-o'-Death (RFC 5905 §7.4): какому серверу ещё
//! можно слать запрос.
//!
//! - DENY, RSTR — сервер нам отказал: запросов к нему больше не шлём;
//! - RATE — шлём слишком часто: к этому серверу не раньше чем через интервал,
//!   который удваивается с каждым следующим RATE от него, от
//!   [`RATE_FIRST_INTERVAL`] до [`RATE_MAX_INTERVAL`];
//! - остальные коды протокольного смысла не несут (§7.4) — обычный отказ.
//!
//! Сервер — IP-адрес, не имя: `pool.ntp.org` на каждый DNS-запрос отдаёт адрес
//! из пула, избегать надо конкретный адрес. Память живёт в ОЗУ и после
//! перезагрузки пуста: сервер, который всё ещё против, поцелует снова.

use core::net::IpAddr;
use core::time::Duration;

use heapless::Vec;
use sntpc::KissOfDeathCode;

use crate::supervisor::Uptime;

/// Первый отступ после RATE: 2^6 с — MINPOLL по умолчанию у ntpd.
pub const RATE_FIRST_INTERVAL: Duration = Duration::from_secs(1 << 6);
/// Предел отступа: 2^17 с ≈ 36 ч — MAXPOLL RFC 5905.
pub const RATE_MAX_INTERVAL: Duration = Duration::from_secs(1 << 17);

/// Сколько серверов помнить. При переполнении забывается давнее всех
/// поцеловавший: если это был DENY, к нему уйдёт ещё один запрос и он
/// поцелует снова.
pub const KISS_MEMORY: usize = 8;

/// Почему запрос серверу не шлём.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shunned {
    /// DENY или RSTR: больше никогда (в эту загрузку).
    Denied,
    /// RATE: не раньше `until`.
    RateLimited { until: Uptime },
}

#[derive(Clone, Copy, Debug)]
enum Verdict {
    Denied,
    /// Отступ, назначенный последним RATE.
    RateLimited {
        interval: Duration,
    },
}

#[derive(Clone, Copy, Debug)]
struct Kissed {
    server:  IpAddr,
    /// Момент последнего `KoD` от сервера.
    at:      Uptime,
    verdict: Verdict,
}

impl Kissed {
    const fn shunned(&self) -> Shunned {
        match self.verdict {
            Verdict::Denied => Shunned::Denied,
            Verdict::RateLimited { interval } => {
                Shunned::RateLimited {
                    until: Uptime(self.at.0.saturating_add(interval)),
                }
            }
        }
    }
}

/// Серверы, приславшие `KoD` со смыслом (DENY, RSTR, RATE).
#[derive(Debug, Default)]
pub struct KissMemory {
    kissed: Vec<Kissed, KISS_MEMORY>,
}

impl KissMemory {
    #[must_use]
    pub const fn new() -> Self {
        Self { kissed: Vec::new() }
    }

    /// Можно ли в момент `now` слать запрос `server`.
    ///
    /// # Errors
    ///
    /// [`Shunned`], если сервер отказал нам или его отступ после RATE ещё идёт.
    pub fn admit(&self, server: IpAddr, now: Uptime) -> Result<(), Shunned> {
        match self.find(server).map(Kissed::shunned) {
            Some(Shunned::RateLimited { until }) if until <= now => Ok(()),
            Some(shunned) => Err(shunned),
            None => Ok(()),
        }
    }

    /// Учесть `KoD` `code` от `server`, полученный в момент `now`.
    ///
    /// Возвращает, что теперь с сервером, или `None`, если код протокольного
    /// смысла не несёт и запрос — просто неудачный.
    pub fn kissed(&mut self, server: IpAddr, code: KissOfDeathCode, now: Uptime) -> Option<Shunned> {
        let previous = self.find(server).map(|kissed| kissed.verdict);
        let verdict = match code {
            KissOfDeathCode::Deny | KissOfDeathCode::Rstr => Verdict::Denied,
            KissOfDeathCode::Rate => {
                match previous {
                    // Отказавший сервер отказывает и дальше: RATE его не снимает.
                    Some(Verdict::Denied) => Verdict::Denied,
                    Some(Verdict::RateLimited { interval }) => {
                        Verdict::RateLimited {
                            interval: interval.saturating_mul(2).min(RATE_MAX_INTERVAL),
                        }
                    }
                    None => {
                        Verdict::RateLimited {
                            interval: RATE_FIRST_INTERVAL,
                        }
                    }
                }
            }
            _ => return None,
        };
        let kissed = Kissed {
            server,
            at: now,
            verdict,
        };
        let shunned = kissed.shunned();
        self.remember(kissed);
        Some(shunned)
    }

    fn find(&self, server: IpAddr) -> Option<&Kissed> {
        self.kissed.iter().find(|kissed| kissed.server == server)
    }

    fn remember(&mut self, kissed: Kissed) {
        if let Some(slot) = self.kissed.iter_mut().find(|known| known.server == kissed.server) {
            *slot = kissed;
            return;
        }
        if self.kissed.is_full() {
            let oldest = self
                .kissed
                .iter()
                .enumerate()
                .min_by_key(|(_, known)| known.at)
                .map(|(index, _)| index)
                .expect("a full memory is not empty");
            self.kissed.swap_remove(oldest);
        }
        self.kissed
            .push(kissed)
            .expect("a slot was freed above if the memory was full");
    }
}
