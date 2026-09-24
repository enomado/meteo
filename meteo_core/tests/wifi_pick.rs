use meteo_core::wifi_pick::{
    Network,
    Rssi,
    round_robin,
    strongest,
};

const HOME: Network<'static> = Network {
    ssid:     "home",
    password: "pw1",
};
const DACHA: Network<'static> = Network {
    ssid:     "dacha",
    password: "pw2",
};
const NETS: [Network<'static>; 2] = [HOME, DACHA];

#[test]
fn strongest_configured_network_wins() {
    let seen = [
        ("neighbour", Rssi(-30)),
        ("home", Rssi(-70)),
        ("dacha", Rssi(-55)),
    ];
    assert_eq!(
        strongest(&NETS, seen),
        Some(meteo_core::wifi_pick::NetworkSignal {
            network: DACHA,
            rssi:    Rssi(-55),
        })
    );
}

#[test]
fn foreign_ssids_are_ignored() {
    let seen = [("neighbour", Rssi(-20)), ("cafe", Rssi(-40))];
    assert_eq!(strongest(&NETS, seen), None);
    assert_eq!(strongest(&NETS, []), None);
}

#[test]
fn several_aps_of_one_network_take_the_strongest() {
    let seen = [("home", Rssi(-80)), ("dacha", Rssi(-60)), ("home", Rssi(-50))];
    assert_eq!(
        strongest(&NETS, seen),
        Some(meteo_core::wifi_pick::NetworkSignal {
            network: HOME,
            rssi:    Rssi(-50),
        })
    );
}

#[test]
fn tie_keeps_the_first_seen() {
    let seen = [("dacha", Rssi(-60)), ("home", Rssi(-60))];
    assert_eq!(
        strongest(&NETS, seen),
        Some(meteo_core::wifi_pick::NetworkSignal {
            network: DACHA,
            rssi:    Rssi(-60),
        })
    );
}

#[test]
fn round_robin_cycles_through_all_networks() {
    let picked: Vec<_> = (0..5).map(|a| round_robin(&NETS, a).ssid).collect();
    assert_eq!(picked, ["home", "dacha", "home", "dacha", "home"]);
    assert_eq!(round_robin(&[HOME], usize::MAX), HOME);
}
