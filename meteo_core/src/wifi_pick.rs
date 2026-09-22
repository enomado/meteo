//! Выбор WiFi-сети из конфига по результатам скана. Скан — в прошивке, здесь
//! только решение.

/// Сила сигнала точки доступа, dBm (как отдаёт скан: отрицательное число, чем
/// ближе к нулю — тем сильнее).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Rssi(pub i8);

/// Сеть из конфига прошивки: `(ssid, пароль)`.
pub type Network<'n> = (&'n str, &'n str);

/// Настроенная сеть с самым сильным сигналом среди увиденных точек доступа.
/// Чужие SSID игнорируются; у сети с несколькими AP берётся сильнейшая. При
/// равном сигнале побеждает увиденная первой (скан сортирует по RSSI).
/// `None` — ни одной своей сети в эфире.
pub fn strongest<'n, 's>(
    networks: &'n [Network<'n>],
    seen: impl IntoIterator<Item = (&'s str, Rssi)>,
) -> Option<(&'n Network<'n>, Rssi)> {
    let mut best: Option<(&'n Network<'n>, Rssi)> = None;
    for (ssid, rssi) in seen {
        let Some(net) = networks.iter().find(|(own, _)| *own == ssid) else {
            continue;
        };
        if best.is_none_or(|(_, best_rssi)| rssi > best_rssi) {
            best = Some((net, rssi));
        }
    }
    best
}

/// Сеть для попытки `attempt`, когда скан не помог (упал или своих SSID нет):
/// перебор по кругу, а не липнем к первой — иначе при сломанном скане вторая
/// сеть не была бы испробована никогда. Предусловие: `networks` не пуст
/// (build-скрипт прошивки кладёт в конфиг минимум одну сеть).
pub fn round_robin<'n>(networks: &'n [Network<'n>], attempt: usize) -> &'n Network<'n> {
    &networks[attempt % networks.len()]
}
