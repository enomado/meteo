#![no_std]
#![no_main]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]
// #![feature(type_alias_impl_trait)]

use esp_hal::interrupt::software::{SoftwareInterrupt, SoftwareInterruptControl};
use esp_hal::{clock::CpuClock, rng::Rng, timer::timg::TimerGroup};

use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};

// если принтлн то будет плакать если не закомментить
// use defmt as _;

// duplicate symbol, because it defined in esp-hal?
// если defmt выключен то это просто затычка чтобы линкер не плакал
// use defmt_rtt as _;

use esp_println as _;

//
use esp_backtrace as _;

use esp_alloc as _;
// use esp_preempt as _;

use esp_radio::wifi::WifiController;
use esp_rtos as _;

esp_bootloader_esp_idf::esp_app_desc!();

use embassy_sync::mutex::Mutex;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use embassy_time::Delay;
use meteo::mk_static;
use meteo::ntp_client::ntp_sync_loop;
use meteo::{
    network::{connection, net_task, network_send_loop},
    ntp_client::CURRENT_OFFSET,
    sensor::sensor_loop_new,
};

use embassy_executor::raw::Executor;

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // defmt::init?
    // esp_println::logger::init_logger_from_env();

    // let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::default());

    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    // peripherals.RNG
    let mut rng = Rng::new();

    // let esp_radio_ctrl = init_esp_radio();
    // let esp_radio_ctrl = &*mk_static!(WifiController<'static>, esp_radio::init().unwrap());

    let (controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).unwrap();

    // WifiMode::default()

    // let (controller, interfaces) = esp_radio::wifi::new(
    //     esp_radio_ctrl,
    //     peripherals.WIFI,
    //     // esp_radio::wifi::Config::default(),
    // )
    // .unwrap();

    let wifi_interface = interfaces.station;

    // {
    //         let timg1 = TimerGroup::new(peripherals.TIMG1);
    //         esp_hal_embassy::init(timg1.timer0);
    // }
    {
        use esp_hal::timer::systimer::SystemTimer;
        let systimer = SystemTimer::new(peripherals.SYSTIMER);
        // esp_rtos::start(systimer.alarm0);
    }

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<7>, StackResources::<7>::new()),
        seed,
    );

    // CURRENT_OFFSET.init(1757985960);

    spawner.spawn(net_task(runner)).ok();
    spawner.spawn(connection(controller)).ok();

    spawner.spawn(sensor_loop_new()).ok();

    let spawner_w = spawner.clone();
    spawner.spawn(ntp_sync_loop(stack, spawner_w)).ok();

    spawner.spawn(network_send_loop(stack)).ok();

    loop {
        Timer::after(Duration::from_millis(5000)).await;
    }
}
