use std::collections::BTreeSet;

use meteo_core::datagram::Seq;
use meteo_core::recv_window::{
    RecvWindow,
    SeqStatus,
    WINDOW_BITS,
};
use proptest::collection::vec;
use proptest::prelude::*;

#[test]
fn window_edges() {
    let mut w = RecvWindow::new(Seq(100));
    assert!(w.mark(Seq(100 - WINDOW_BITS)), "lowest in-window seq");
    assert!(!w.mark(Seq(99 - WINDOW_BITS)), "first seq below the window");
    assert_eq!(w.status(Seq(100 - WINDOW_BITS)), SeqStatus::Seen);
    assert_eq!(w.status(Seq(99 - WINDOW_BITS)), SeqStatus::OutOfWindow);
    assert_eq!(w.status(Seq(99)), SeqStatus::NotSeen);
    assert_eq!(w.status(Seq(101)), SeqStatus::Ahead);

    // Сдвиг ровно на ширину окна: прежний highest — на последнем бите.
    let mut w = RecvWindow::new(Seq(10));
    w.mark(Seq(10 + WINDOW_BITS));
    assert_eq!(w.status(Seq(10)), SeqStatus::Seen);
    // На единицу больше — выпадает.
    let mut w = RecvWindow::new(Seq(10));
    w.mark(Seq(11 + WINDOW_BITS));
    assert_eq!(w.status(Seq(10)), SeqStatus::OutOfWindow);
}

proptest! {
    /// Окно против эталона-множества: статус любого номера совпадает.
    #[test]
    fn matches_set_model(first in 1u32..1000, steps in vec(0u32..80, 1..200)) {
        let mut w = RecvWindow::new(Seq(first));
        let mut marked = BTreeSet::from([first]);
        let mut highest = first;
        for step in steps {
            // Смесь: вперёд (step ≥ 40) и назад относительно highest.
            let seq = if step >= 40 { highest + (step - 40) } else { highest.saturating_sub(step).max(1) };
            let in_window = seq > highest || highest - seq <= WINDOW_BITS;
            prop_assert_eq!(w.mark(Seq(seq)), in_window);
            if in_window {
                marked.insert(seq);
                highest = highest.max(seq);
            }
        }
        prop_assert_eq!(w.highest(), Seq(highest));
        for seq in highest.saturating_sub(WINDOW_BITS + 5).max(1)..highest + 5 {
            let expect = if seq > highest {
                SeqStatus::Ahead
            } else if highest - seq > WINDOW_BITS {
                SeqStatus::OutOfWindow
            } else if marked.contains(&seq) {
                SeqStatus::Seen
            } else {
                SeqStatus::NotSeen
            };
            prop_assert_eq!(w.status(Seq(seq)), expect, "seq {}", seq);
        }
    }
}
