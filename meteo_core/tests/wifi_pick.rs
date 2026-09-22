use meteo_core::wifi_pick::{
    Network,
    Rssi,
    round_robin,
    strongest,
};

const HOME: Network<'static> = ("home", "pw1");
const DACHA: Network<'static> = ("dacha", "pw2");
const NETS: [Network<'static>; 2] = [HOME, DACHA];

#[test]
fn strongest_configured_network_wins() {
    let seen = [
        ("neighbour", Rssi(-30)),
        ("home", Rssi(-70)),
        ("dacha", Rssi(-55)),
    ];
    assert_eq!(strongest(&NETS, seen), Some((&DACHA, Rssi(-55))));
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
    assert_eq!(strongest(&NETS, seen), Some((&HOME, Rssi(-50))));
}

#[test]
fn tie_keeps_the_first_seen() {
    let seen = [("dacha", Rssi(-60)), ("home", Rssi(-60))];
    assert_eq!(strongest(&NETS, seen), Some((&DACHA, Rssi(-60))));
}

#[test]
fn round_robin_cycles_through_all_networks() {
    let picked: Vec<_> = (0..5).map(|a| round_robin(&NETS, a).0).collect();
    assert_eq!(picked, ["home", "dacha", "home", "dacha", "home"]);
    assert_eq!(round_robin(&[HOME], usize::MAX), &HOME);
}
