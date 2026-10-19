//! Повтор подключения к Wi-Fi и LED-код «нет Wi-Fi». Радио — в прошивке, здесь
//! только решение: когда повторять и когда поднимать код.
//!
//! Плата умирает молча (reset, снятое питание) и не шлёт точке доступа deauth.
//! Пока у точки жива сессия прошлой жизни с тем же MAC, первая попытка
//! получает от неё сброс сессии с причиной [`ReasonCode::AUTH_EXPIRED`], а
//! вторая проходит (плата, 2026-10-18). Такую неудачу повторяем сразу и кодом
//! не пугаем: код поднимается со второй неудачи подряд.

use core::time::Duration;

/// Код причины отключения от драйвера радио: номер IEEE 802.11 (1–68) или
/// ESP (200+).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReasonCode(pub u16);

impl ReasonCode {
    /// IEEE 802.11: «Previous authentication no longer valid» — точка доступа
    /// сбросила оставшуюся у неё сессию этого MAC.
    pub const AUTH_EXPIRED: Self = Self(2);
}

/// Пауза перед следующей попыткой после обрыва или обычной неудачи.
pub const RETRY_PAUSE: Duration = Duration::from_secs(5);

/// Столько неудачных попыток подряд ⇒ LED-код «нет Wi-Fi».
pub const FAILURES_FOR_CODE: u32 = 2;

/// Что делать после неудачной попытки.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verdict {
    /// Пауза до следующей попытки.
    pub pause:        Duration,
    /// Поднять LED-код «нет Wi-Fi».
    pub raise_status: bool,
}

/// Счётчик неудачных попыток подряд с последнего подключения или обрыва.
#[derive(Debug, Default)]
pub struct Reconnect {
    failures: u32,
}

impl Reconnect {
    #[must_use]
    pub const fn new() -> Self {
        Self { failures: 0 }
    }

    /// Подключились.
    pub const fn on_connected(&mut self) {
        self.failures = 0;
    }

    /// Обрыв установленного соединения. Код «нет Wi-Fi» прошивка поднимает
    /// сразу: связи нет; повтор — через [`RETRY_PAUSE`]. Попытки после обрыва
    /// считаются заново, и первую неудачу с [`ReasonCode::AUTH_EXPIRED`] снова
    /// повторяем сразу.
    pub const fn on_dropped(&mut self) {
        self.failures = 0;
    }

    /// Попытка не удалась. `reason` — `None`, если драйвер отказал, не дойдя
    /// до точки доступа (события отключения не было).
    pub const fn on_failed(&mut self, reason: Option<ReasonCode>) -> Verdict {
        self.failures = self.failures.saturating_add(1);
        let stale_session = matches!(reason, Some(ReasonCode::AUTH_EXPIRED)) && self.failures == 1;
        Verdict {
            pause:        if stale_session {
                Duration::ZERO
            } else {
                RETRY_PAUSE
            },
            raise_status: self.failures >= FAILURES_FOR_CODE,
        }
    }
}
