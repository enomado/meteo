//! Окно приёма: какие датаграммы одной загрузки прошивки записаны в базу.
//!
//! Приёмник держит окно на каждый `BootId` и шлёт его снимок в подтверждении;
//! прошивка по тому же снимку решает судьбу своих датаграмм в полёте. Логика
//! одна на обе стороны — поэтому она здесь, а не в приёмнике.
//!
//! Окно = наибольший записанный `Seq` + 32 бита о предыдущих: бит `i` стоит ⇔
//! записана датаграмма `highest − 1 − i`. Более старые номера окно не помнит.

use core::cmp::Ordering;

use crate::datagram::Seq;

/// Сколько номеров ниже `highest` помнит окно.
pub const WINDOW_BITS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvWindow {
    highest: Seq,
    seen:    u32,
}

/// Что окно знает о номере датаграммы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqStatus {
    /// Записана в базу.
    Seen,
    /// Ниже `highest`, в пределах окна, не записана: потеряна, отвергнута или
    /// ещё едет (перестановка).
    NotSeen,
    /// Ниже окна: судьба неизвестна.
    OutOfWindow,
    /// Выше `highest`: сервер о ней ещё не знает.
    Ahead,
}

impl RecvWindow {
    /// Окно после записи первой датаграммы загрузки.
    #[must_use]
    pub const fn new(first: Seq) -> Self {
        Self {
            highest: first,
            seen:    0,
        }
    }

    /// Окно из подтверждения (поля как на проводе).
    #[must_use]
    pub const fn from_parts(highest: Seq, seen: u32) -> Self {
        Self { highest, seen }
    }

    #[must_use]
    pub const fn highest(&self) -> Seq {
        self.highest
    }

    #[must_use]
    pub const fn seen_bits(&self) -> u32 {
        self.seen
    }

    #[must_use]
    pub fn status(&self, seq: Seq) -> SeqStatus {
        if seq == self.highest {
            return SeqStatus::Seen;
        }
        if seq > self.highest {
            return SeqStatus::Ahead;
        }
        let back = self.highest.0 - seq.0 - 1;
        if back >= WINDOW_BITS {
            SeqStatus::OutOfWindow
        } else if self.seen & (1 << back) != 0 {
            SeqStatus::Seen
        } else {
            SeqStatus::NotSeen
        }
    }

    /// Отметить датаграмму записанной — ТОЛЬКО после успешной записи в базу.
    /// `false` — номер ниже окна, отметить нечем (запись всё равно
    /// идемпотентна, прошивка переотправит показания под новым номером).
    pub fn mark(&mut self, seq: Seq) -> bool {
        if seq > self.highest {
            let shift = seq.0 - self.highest.0;
            // Прежний highest встаёт на бит shift − 1, остальные едут следом.
            // При сдвиге ровно на ширину окна остаётся только этот последний бит.
            self.seen = match shift.cmp(&WINDOW_BITS) {
                Ordering::Greater => 0,
                Ordering::Equal => 1 << (WINDOW_BITS - 1),
                Ordering::Less => (self.seen << shift) | (1 << (shift - 1)),
            };
            self.highest = seq;
            return true;
        }
        if seq == self.highest {
            return true;
        }
        let back = self.highest.0 - seq.0 - 1;
        if back >= WINDOW_BITS {
            return false;
        }
        self.seen |= 1 << back;
        true
    }
}
