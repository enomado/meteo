// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html

#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// fn init_esp_radio() -> &'static mut Controller<'static> {
//     // let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());
//     // let esp_radio_ctrl = static_cell::make_static!(esp_radio::init().unwrap());
//     static_cell::make_static!(esp_radio::init().unwrap())
// }
