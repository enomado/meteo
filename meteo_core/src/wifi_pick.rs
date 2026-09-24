//! Выбор WiFi-сети из конфига по результатам скана. Скан — в прошивке, здесь
//! только решение.

/// Сила сигнала точки доступа, dBm (как отдаёт скан: отрицательное число, чем
/// ближе к нулю — тем сильнее).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Rssi(pub i8);

/// Сеть из конфига прошивки.
///
/// Именованные поля не позволяют незаметно перепутать SSID и пароль при
/// настройке подключения.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Network<'n> {
    pub ssid:     &'n str,
    pub password: &'n str,
}

/// Настроенная сеть и сигнал сильнейшей найденной для неё точки доступа.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NetworkSignal<'n> {
    pub network: Network<'n>,
    pub rssi:    Rssi,
}

/// Настроенная сеть с самым сильным сигналом среди увиденных точек доступа.
///
/// Чужие SSID игнорируются; у сети с несколькими AP берётся сильнейшая. При
/// равном сигнале побеждает увиденная первой (скан сортирует по RSSI).
/// `None` — ни одной своей сети в эфире.
#[must_use]
pub fn strongest<'n, 's>(
    networks: &[Network<'n>],
    seen: impl IntoIterator<Item = (&'s str, Rssi)>,
) -> Option<NetworkSignal<'n>> {
    let mut best: Option<NetworkSignal<'n>> = None;
    for (ssid, rssi) in seen {
        let Some(net) = networks.iter().find(|network| network.ssid == ssid) else {
            continue;
        };
        if best.is_none_or(|best| rssi > best.rssi) {
            best = Some(NetworkSignal { network: *net, rssi });
        }
    }
    best
}

/// Сеть для попытки `attempt`, когда скан не помог (упал или своих SSID нет).
///
/// Перебор идёт по кругу — иначе при сломанном скане вторая сеть не была бы
/// испробована никогда. `networks` должен быть непустым; build-скрипт прошивки
/// кладёт в конфиг минимум одну сеть.
#[must_use]
pub const fn round_robin<'n>(networks: &[Network<'n>], attempt: usize) -> Network<'n> {
    networks[attempt % networks.len()]
}
