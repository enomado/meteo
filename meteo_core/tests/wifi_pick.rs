use meteo_core::wifi_pick::{
    Network,
    Rssi,
    ScannedAccessPoint,
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
        ScannedAccessPoint {
            ssid: "neighbour",
            rssi: Rssi(-30),
        },
        ScannedAccessPoint {
            ssid: "home",
            rssi: Rssi(-70),
        },
        ScannedAccessPoint {
            ssid: "dacha",
            rssi: Rssi(-55),
        },
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
    let seen = [
        ScannedAccessPoint {
            ssid: "neighbour",
            rssi: Rssi(-20),
        },
        ScannedAccessPoint {
            ssid: "cafe",
            rssi: Rssi(-40),
        },
    ];
    assert_eq!(strongest(&NETS, seen), None);
    assert_eq!(strongest(&NETS, []), None);
}

#[test]
fn several_aps_of_one_network_take_the_strongest() {
    let seen = [
        ScannedAccessPoint {
            ssid: "home",
            rssi: Rssi(-80),
        },
        ScannedAccessPoint {
            ssid: "dacha",
            rssi: Rssi(-60),
        },
        ScannedAccessPoint {
            ssid: "home",
            rssi: Rssi(-50),
        },
    ];
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
    let seen = [
        ScannedAccessPoint {
            ssid: "dacha",
            rssi: Rssi(-60),
        },
        ScannedAccessPoint {
            ssid: "home",
            rssi: Rssi(-60),
        },
    ];
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
