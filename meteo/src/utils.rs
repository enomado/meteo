// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html

/// Кладёт значение в `static` и отдаёт `&'static mut` — для ресурсов, которые
/// embassy требует по `'static` (StackResources, LEDC, SPI-шина).
///
/// Каждая инстанциация макроса заводит СВОЙ `StaticCell`, поэтому повторный
/// проход через одно и то же место паникует: вызывать один раз на старте.
#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init($val)
    }};
}
