//! DST: общая SPI-шина прошивки — настоящий `SpiDevice` embassy-embedded-hal
//! против модели шины и CS-пинов. Транзакции отменяются по таймауту посреди
//! любого ожидания, как `with_timeout` в `meteo/src/sensor.rs`. Всё
//! детерминировано: виртуальные часы = опросы, proptest с фиксированным seed'ом.
//!
//! Модель: на одной шине два устройства (в прошивке одно — барометр; второе
//! даёт конкуренцию за мьютекс и свидетеля чужого CS). Транзакции — формы
//! драйвера bmp390-rs: чтение регистра (адрес|0x80, затем байты под заглушку
//! MOSI) и запись пар адрес–данные. Байт уходит на провод за один опрос;
//! операция шины может задержаться, упасть после k байт или зависнуть навсегда,
//! `flush` — задержаться, упасть или зависнуть. Отмена посреди операции
//! обрывает её (байт, не дошедший до провода, не уходит). Устройство на той
//! стороне видит кадр между спадом и фронтом своего CS.
//!
//! Инварианты:
//! 1. после транзакции — завершённой, упавшей или отменённой — CS устройства
//!    поднят;
//! 2. CS опускается и поднимается только под захваченным мьютексом шины, два
//!    CS одновременно не опущены, байт без опущенного CS на провод не уходит;
//! 3. транзакция даёт не больше одного кадра; кадр — префикс её байтов, у
//!    успешной — ровно её байты: обрыв не склеивается со следующей;
//! 4. результат честный: `Ok` ⇔ все операции и `flush` прошли; ошибка шины —
//!    `SpiDeviceError::Spi`; зависание кончается только отменой;
//! 5. мьютекс не утекает: после любой истории каждое устройство проводит
//!    транзакцию без сбоев.
//!
//! Регрессия: та же транзакция без защиты CS при отмене (`OnDrop` в
//! `shared_bus/asynch/spi.rs`) валит инвариант 1 — тест держит, зачем защита.

#![allow(
    clippy::future_not_send,
    reason = "single-threaded simulation; the shared wire model is a RefCell"
)]

use core::cell::RefCell;
use core::convert::Infallible;
use core::future::{
    Future,
    pending,
    poll_fn,
};
use core::pin::Pin;
use core::task::{
    Context,
    Poll,
    Waker,
};
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

use embassy_embedded_hal::shared_bus::SpiDeviceError;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{
    ErrorKind,
    ErrorType,
    Operation,
};
use embedded_hal_async::spi::{
    SpiBus,
    SpiDevice as _,
};
use proptest::collection::vec;
use proptest::prelude::{
    Just,
    Strategy,
    any,
    prop_oneof,
};
use proptest::test_runner::{
    Config,
    RngSeed,
    TestCaseError,
    TestRunner,
};

const DEVICES: usize = 2;
/// Что шина шлёт по MOSI, читая (esp-hal `EMPTY_WRITE_PAD`).
const READ_PAD: u8 = 0x00;
/// Что модель устройства отвечает по MISO; данные чтения тест не проверяет.
const MISO_FILL: u8 = 0x5a;
/// Предел опросов на сценарий: дольше — значит, кто-то ждёт мьютекс вечно.
const STEP_LIMIT: u32 = 100_000;

/// Чем кончается операция шины.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    /// Ошибка после `after` байт.
    Fail,
    /// После `after` байт шина не отвечает никогда.
    Hang,
}

#[derive(Clone, Copy, Debug)]
struct OpPlan {
    /// Опросов до первого байта.
    lead:    u8,
    outcome: Outcome,
    /// Сколько байт успевает уйти до сбоя (для `Done` не важно).
    after:   u8,
}

impl OpPlan {
    const CLEAN: Self = Self {
        lead:    0,
        outcome: Outcome::Done,
        after:   0,
    };
}

/// Сценарий шины для одной транзакции: операции по порядку, затем `flush`.
/// Операций в плане две — столько у самой длинной формы; лишняя не тратится.
#[derive(Clone, Copy, Debug)]
struct TxPlan {
    ops:   [OpPlan; 2],
    flush: OpPlan,
}

impl TxPlan {
    const CLEAN: Self = Self {
        ops:   [OpPlan::CLEAN; 2],
        flush: OpPlan::CLEAN,
    };

    /// Чем обязана кончиться транзакция из `ops` операций при этом плане:
    /// после сбоя операции `SpiDevice` всё равно делает `flush`.
    fn fate(&self, ops: usize) -> Fate {
        let first_fault = self.ops[..ops]
            .iter()
            .map(|op| op.outcome)
            .find(|outcome| *outcome != Outcome::Done);
        match (first_fault, self.flush.outcome) {
            (Some(Outcome::Hang), _) | (_, Outcome::Hang) => Fate::Hangs,
            (Some(_), _) | (None, Outcome::Fail) => Fate::Fails,
            (None, Outcome::Done) => Fate::Succeeds,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Succeeds,
    Fails,
    Hangs,
}

/// Где транзакция стояла в момент отмены.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Ждала мьютекс шины (до CS).
    Lock,
    /// Операция ещё не начала слать байты.
    Lead,
    /// Посреди байтов операции.
    Clocking,
    /// Операция зависла.
    Hung,
    /// Внутри `flush`.
    Flush,
}

const PHASES: [Phase; 5] = [
    Phase::Lock,
    Phase::Lead,
    Phase::Clocking,
    Phase::Hung,
    Phase::Flush,
];

/// Транзакция формы драйвера bmp390-rs.
#[derive(Clone, Debug)]
enum Shape {
    /// `[Write(addr | 0x80), Read(len)]`, `len` — с фиктивным байтом.
    ReadReg { addr: u8, len: usize },
    /// `[Write(пары адрес–данные)]`.
    WriteRegs(Vec<u8>),
}

impl Shape {
    const fn ops(&self) -> usize {
        match self {
            Self::ReadReg { .. } => 2,
            Self::WriteRegs(_) => 1,
        }
    }

    /// Байты, которые устройство увидит по MOSI за всю транзакцию.
    fn mosi(&self) -> Vec<u8> {
        match self {
            Self::ReadReg { addr, len } => {
                let mut frame = vec![addr | 0x80];
                frame.resize(1 + len, READ_PAD);
                frame
            }
            Self::WriteRegs(bytes) => bytes.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct Attempt {
    shape:  Shape,
    plan:   TxPlan,
    /// Таймаут транзакции в опросах.
    budget: u32,
}

#[derive(Clone, Debug)]
struct Scenario {
    attempts: [Vec<Attempt>; DEVICES],
    /// Порядок опроса задач по шагам, по кругу: 0 — A,B; 1 — B,A; 2 — только A;
    /// 3 — только B.
    schedule: Vec<u8>,
}

/// Провод и всё, что видно снаружи `SpiDevice`: уровни CS, кадры на стороне
/// устройств, план сбоев шины.
///
/// `RefCell`: шину и пины держит `SpiDevice` (по значению и за мьютексом),
/// а модель провода у них общая — иначе пин не видит шину и наоборот.
#[derive(Default)]
struct WireState {
    cs_low:     [bool; DEVICES],
    frames:     [Vec<Vec<u8>>; DEVICES],
    ops:        [VecDeque<OpPlan>; DEVICES],
    flush:      [Option<OpPlan>; DEVICES],
    /// Последнее ожидание транзакции; `None` — CS ещё не опускался.
    phase:      [Option<Phase>; DEVICES],
    violations: Vec<String>,
    cancels:    [u32; PHASES.len()],
    completed:  u32,
}

#[derive(Default)]
struct Wire(RefCell<WireState>);

impl Wire {
    fn violation(&self, message: String) {
        self.0.borrow_mut().violations.push(message);
    }

    /// Устройство, чей CS опущен, — то, с кем шина сейчас говорит. Второй
    /// опущенный CS уже записан нарушением в `set_cs`.
    fn owner(&self) -> Option<usize> {
        let state = self.0.borrow();
        (0..DEVICES).find(|dev| state.cs_low[*dev])
    }

    fn set_cs(&self, dev: usize, low: bool) {
        let mut state = self.0.borrow_mut();
        if low && !state.cs_low[dev] {
            if let Some(other) = (0..DEVICES).find(|other| *other != dev && state.cs_low[*other]) {
                state
                    .violations
                    .push(format!("CS {dev} lowered while CS {other} is low"));
            }
            state.frames[dev].push(Vec::new());
        }
        state.cs_low[dev] = low;
    }

    /// Байт ушёл на провод: его видят все устройства с опущенным CS.
    fn clock(&self, byte: u8) {
        let mut state = self.0.borrow_mut();
        let mut seen = false;
        for dev in 0..DEVICES {
            if state.cs_low[dev] {
                state.frames[dev]
                    .last_mut()
                    .expect("a low CS has an open frame")
                    .push(byte);
                seen = true;
            }
        }
        if !seen {
            state
                .violations
                .push(format!("byte {byte:#04x} clocked with every CS high"));
        }
    }

    fn set_phase(&self, dev: usize, phase: Phase) {
        self.0.borrow_mut().phase[dev] = Some(phase);
    }

    /// План для очередной операции хозяина шины.
    fn begin_op(&self) -> Option<(usize, OpPlan)> {
        let Some(dev) = self.owner() else {
            self.violation("bus operation with every CS high".into());
            return None;
        };
        // Пусто только у чужого плана: два CS опущены, и это уже нарушение.
        let plan = self.0.borrow_mut().ops[dev].pop_front();
        Some((dev, plan.unwrap_or(OpPlan::CLEAN)))
    }

    fn begin_flush(&self) -> Option<(usize, OpPlan)> {
        let Some(dev) = self.owner() else {
            self.violation("bus flush with every CS high".into());
            return None;
        };
        let plan = self.0.borrow_mut().flush[dev].take();
        if plan.is_none() {
            self.violation(format!("second flush in a transaction of device {dev}"));
        }
        Some((dev, plan.unwrap_or(OpPlan::CLEAN)))
    }

    fn load(&self, dev: usize, plan: &TxPlan) {
        let mut state = self.0.borrow_mut();
        state.ops[dev] = plan.ops.into_iter().collect();
        state.flush[dev] = Some(plan.flush);
        state.phase[dev] = None;
    }

    fn frame_count(&self, dev: usize) -> usize {
        self.0.borrow().frames[dev].len()
    }

    /// Инварианты 1, 3, 4 для завершившейся попытки.
    fn check_attempt(
        &self,
        dev: usize,
        frames_before: usize,
        attempt: &Attempt,
        result: &Result<Result<(), DeviceError>, Elapsed>,
    ) {
        let mut state = self.0.borrow_mut();
        let mut messages = Vec::new();
        if state.cs_low[dev] {
            messages.push(format!("CS still low after {result:?}"));
        }

        let expected = attempt.shape.mosi();
        let frames = &state.frames[dev][frames_before..];
        match frames {
            [] => {
                if !matches!(result, Err(Elapsed)) || state.phase[dev].is_some() {
                    messages.push(format!("no frame for {result:?}"));
                }
            }
            [frame] => {
                if !expected.starts_with(frame) {
                    messages.push(format!("frame {frame:02x?} is not a prefix of {expected:02x?}"));
                }
                if matches!(result, Ok(Ok(()))) && *frame != expected {
                    messages.push(format!("successful frame {frame:02x?} != {expected:02x?}"));
                }
            }
            _ => messages.push(format!("{} frames for one transaction", frames.len())),
        }

        let fate = attempt.plan.fate(attempt.shape.ops());
        match (result, fate) {
            (Ok(Ok(())), Fate::Succeeds) | (Ok(Err(SpiDeviceError::Spi(BusFault))), Fate::Fails) => {
                state.completed += 1;
            }
            (Err(Elapsed), _) => {
                let phase = state.phase[dev].unwrap_or(Phase::Lock);
                let index = PHASES
                    .iter()
                    .position(|p| *p == phase)
                    .expect("phase from PHASES");
                state.cancels[index] += 1;
            }
            (Ok(_), _) => messages.push(format!("result {result:?} for a {fate:?} plan")),
        }

        for message in messages {
            state
                .violations
                .push(format!("device {dev}, {attempt:?}: {message}"));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BusFault;

impl embedded_hal::spi::Error for BusFault {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// Модель SPI-периферии: байт за опрос, сбои по плану хозяина шины.
struct SimBus<'a> {
    wire: &'a Wire,
}

/// Ровно один `Pending`: шаг виртуальных часов.
async fn tick() {
    let mut yielded = false;
    poll_fn(|_| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            Poll::Pending
        }
    })
    .await;
}

impl SimBus<'_> {
    async fn clock_out(&self, mosi: Vec<u8>) -> Result<(), BusFault> {
        let Some((dev, plan)) = self.wire.begin_op() else {
            return Ok(());
        };
        self.wire.set_phase(dev, Phase::Lead);
        for _ in 0..plan.lead {
            tick().await;
        }
        let sent = match plan.outcome {
            Outcome::Done => mosi.len(),
            Outcome::Fail | Outcome::Hang => mosi.len().min(usize::from(plan.after)),
        };
        self.wire.set_phase(dev, Phase::Clocking);
        for byte in &mosi[..sent] {
            // Отмена во время такта обрывает передачу: байт не уходит.
            tick().await;
            self.wire.clock(*byte);
        }
        match plan.outcome {
            Outcome::Done => Ok(()),
            Outcome::Fail => Err(BusFault),
            Outcome::Hang => {
                self.wire.set_phase(dev, Phase::Hung);
                pending().await
            }
        }
    }
}

impl ErrorType for SimBus<'_> {
    type Error = BusFault;
}

impl SpiBus<u8> for SimBus<'_> {
    async fn read(&mut self, words: &mut [u8]) -> Result<(), BusFault> {
        self.clock_out(vec![READ_PAD; words.len()]).await?;
        words.fill(MISO_FILL);
        Ok(())
    }

    async fn write(&mut self, words: &[u8]) -> Result<(), BusFault> {
        self.clock_out(words.to_vec()).await
    }

    async fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), BusFault> {
        let mut mosi = write.to_vec();
        mosi.resize(read.len().max(write.len()), READ_PAD);
        self.clock_out(mosi).await?;
        read.fill(MISO_FILL);
        Ok(())
    }

    async fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), BusFault> {
        self.clock_out(words.to_vec()).await?;
        words.fill(MISO_FILL);
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), BusFault> {
        let Some((dev, plan)) = self.wire.begin_flush() else {
            return Ok(());
        };
        self.wire.set_phase(dev, Phase::Flush);
        for _ in 0..plan.lead {
            tick().await;
        }
        match plan.outcome {
            Outcome::Done => Ok(()),
            Outcome::Fail => Err(BusFault),
            Outcome::Hang => pending().await,
        }
    }
}

type Bus<'a> = Mutex<NoopRawMutex, SimBus<'a>>;
type DeviceError = SpiDeviceError<BusFault, Infallible>;

/// CS-пин; как `esp_hal::gpio::Output`, не ошибается. Свидетель инварианта 2:
/// пока CS меняется, мьютекс шины должен быть захвачен.
struct SimCs<'a> {
    dev:  usize,
    wire: &'a Wire,
    bus:  &'a Bus<'a>,
}

impl SimCs<'_> {
    fn set(&self, low: bool) {
        if self.bus.try_lock().is_ok() {
            self.wire.violation(format!(
                "CS {} set {} without holding the bus",
                self.dev,
                if low { "low" } else { "high" }
            ));
        }
        self.wire.set_cs(self.dev, low);
    }
}

impl embedded_hal::digital::ErrorType for SimCs<'_> {
    type Error = Infallible;
}

impl OutputPin for SimCs<'_> {
    fn set_low(&mut self) -> Result<(), Infallible> {
        self.set(true);
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Infallible> {
        self.set(false);
        Ok(())
    }
}

/// `SpiDevice::transaction` из embassy-embedded-hal 0.6 без `OnDrop`-защиты
/// CS — мутант для проверки, что тест её замечает.
struct UnguardedDevice<'a> {
    bus: &'a Bus<'a>,
    cs:  SimCs<'a>,
}

impl UnguardedDevice<'_> {
    #[allow(
        clippy::significant_drop_tightening,
        reason = "mirrors SpiDevice: CS goes high while the bus is still held"
    )]
    async fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), DeviceError> {
        let mut bus = self.bus.lock().await;
        self.cs.set_low().map_err(SpiDeviceError::Cs)?;
        let op_res = 'ops: {
            for op in operations {
                let res = match op {
                    Operation::Read(buf) => bus.read(buf).await,
                    Operation::Write(buf) => bus.write(buf).await,
                    Operation::Transfer(read, write) => bus.transfer(read, write).await,
                    Operation::TransferInPlace(buf) => bus.transfer_in_place(buf).await,
                    Operation::DelayNs(_) => unreachable!("no DelayNs in bmp390-rs transactions"),
                };
                if let Err(e) = res {
                    break 'ops Err(e);
                }
            }
            Ok(())
        };
        let flush_res = bus.flush().await;
        let cs_res = self.cs.set_high();
        op_res.map_err(SpiDeviceError::Spi)?;
        flush_res.map_err(SpiDeviceError::Spi)?;
        cs_res.map_err(SpiDeviceError::Cs)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Настоящий `SpiDevice`, как в прошивке.
    Guarded,
    Unguarded,
}

enum Device<'a> {
    Guarded(SpiDevice<'a, NoopRawMutex, SimBus<'a>, SimCs<'a>>),
    Unguarded(UnguardedDevice<'a>),
}

impl Device<'_> {
    async fn run(&mut self, shape: &Shape) -> Result<(), DeviceError> {
        let mut buf = [0_u8; 16];
        let mut ops: Vec<Operation<'_, u8>> = match shape {
            Shape::ReadReg { addr, len } => {
                vec![
                    Operation::Write(core::slice::from_ref(addr)),
                    Operation::Read(&mut buf[..*len]),
                ]
            }
            Shape::WriteRegs(bytes) => vec![Operation::Write(bytes)],
        };
        match self {
            Self::Guarded(device) => device.transaction(&mut ops).await,
            Self::Unguarded(device) => device.transaction(&mut ops).await,
        }
    }
}

#[derive(Debug)]
struct Elapsed;

/// `embassy_time::with_timeout` на виртуальных часах: бюджет — число опросов.
/// По истечении внутренний future роняется до возврата, как у `select`.
struct WithTimeout<F> {
    inner:      Option<Pin<Box<F>>>,
    polls_left: u32,
}

fn with_timeout<F: Future>(budget: u32, future: F) -> WithTimeout<F> {
    assert!(budget > 0, "a zero timeout never polls the operation");
    WithTimeout {
        inner:      Some(Box::pin(future)),
        polls_left: budget,
    }
}

impl<F: Future> Future for WithTimeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let inner = this.inner.as_mut().expect("polled after completion");
        if let Poll::Ready(output) = inner.as_mut().poll(cx) {
            this.inner = None;
            return Poll::Ready(Ok(output));
        }
        this.polls_left -= 1;
        if this.polls_left == 0 {
            this.inner = None;
            return Poll::Ready(Err(Elapsed));
        }
        Poll::Pending
    }
}

/// Задача одного устройства: попытки по сценарию, затем чистая транзакция без
/// таймаута (инвариант 5: её не дождаться, если мьютекс утёк).
async fn device_task(dev: usize, device: &mut Device<'_>, wire: &Wire, attempts: &[Attempt]) {
    let liveness = Attempt {
        shape:  Shape::ReadReg { addr: 0x80, len: 4 },
        plan:   TxPlan::CLEAN,
        budget: u32::MAX,
    };
    for attempt in attempts.iter().chain([&liveness]) {
        wire.load(dev, &attempt.plan);
        let frames_before = wire.frame_count(dev);
        let result = with_timeout(attempt.budget, device.run(&attempt.shape)).await;
        wire.check_attempt(dev, frames_before, attempt, &result);
        // Цикл датчиков прошивки ждёт таймер между операциями: шина свободна.
        tick().await;
    }
}

struct Report {
    violations: Vec<String>,
    cancels:    [u32; PHASES.len()],
    completed:  u32,
}

fn simulate(kind: Kind, scenario: &Scenario) -> Report {
    let wire = Wire::default();
    let bus: Bus<'_> = Mutex::new(SimBus { wire: &wire });
    let mut devices = [0, 1].map(|dev| {
        let cs = SimCs {
            dev,
            wire: &wire,
            bus: &bus,
        };
        match kind {
            Kind::Guarded => Device::Guarded(SpiDevice::new(&bus, cs)),
            Kind::Unguarded => Device::Unguarded(UnguardedDevice { bus: &bus, cs }),
        }
    });
    let [device_a, device_b] = &mut devices;
    let mut tasks = [
        Box::pin(device_task(0, device_a, &wire, &scenario.attempts[0])),
        Box::pin(device_task(1, device_b, &wire, &scenario.attempts[1])),
    ];

    let mut cx = Context::from_waker(Waker::noop());
    let mut done = [false; DEVICES];
    let mut step = 0;
    while !done.iter().all(|d| *d) {
        if step == STEP_LIMIT {
            wire.violation(format!("tasks {done:?} not finished in {STEP_LIMIT} polls"));
            break;
        }
        let mut order: &[usize] = match scenario.schedule[step as usize % scenario.schedule.len()] {
            0 => &[0, 1],
            1 => &[1, 0],
            2 => &[0],
            _ => &[1],
        };
        // Закончившую задачу опрашивать незачем — шаг достаётся оставшейся.
        if order.iter().all(|dev| done[*dev]) {
            order = &[0, 1];
        }
        for &dev in order {
            if !done[dev] && tasks[dev].as_mut().poll(&mut cx).is_ready() {
                done[dev] = true;
            }
        }
        step += 1;
    }
    drop(tasks);

    if bus.try_lock().is_err() {
        wire.violation("bus mutex still held after every task".into());
    }
    let state = wire.0.into_inner();
    Report {
        violations: state.violations,
        cancels:    state.cancels,
        completed:  state.completed,
    }
}

fn op_plan() -> impl Strategy<Value = OpPlan> {
    let outcome = prop_oneof![
        8 => Just(Outcome::Done),
        1 => Just(Outcome::Fail),
        1 => Just(Outcome::Hang),
    ];
    (0..3_u8, outcome, 0..10_u8).prop_map(|(lead, outcome, after)| OpPlan { lead, outcome, after })
}

fn attempt() -> impl Strategy<Value = Attempt> {
    let shape = prop_oneof![
        (any::<u8>(), 2..=8_usize).prop_map(|(addr, len)| {
            Shape::ReadReg {
                addr: addr | 0x80,
                len,
            }
        }),
        vec(any::<u8>(), 2..=8).prop_map(Shape::WriteRegs),
    ];
    (shape, op_plan(), op_plan(), op_plan(), 1..=30_u32).prop_map(|(shape, first, second, flush, budget)| {
        Attempt {
            shape,
            plan: TxPlan {
                ops: [first, second],
                flush,
            },
            budget,
        }
    })
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (vec(attempt(), 1..12), vec(attempt(), 1..12), vec(0..4_u8, 1..16)).prop_map(|(a, b, schedule)| {
        Scenario {
            attempts: [a, b],
            schedule,
        }
    })
}

fn runner(cases: u32, seed: u64) -> TestRunner {
    TestRunner::new(Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    })
}

/// Инварианты 1–5 под отменой в каждом ожидании транзакции.
#[test]
fn cancelled_transactions_leave_the_bus_clean() {
    let totals = StdMutex::new(([0_u32; PHASES.len()], 0_u32));
    runner(2048, 0x7370_695f_6473_7431)
        .run(&scenario(), |scenario| {
            let report = simulate(Kind::Guarded, &scenario);
            if !report.violations.is_empty() {
                return Err(TestCaseError::fail(report.violations.join("\n")));
            }
            let (cancels, completed) = &mut *totals.lock().expect("not poisoned");
            for (total, count) in cancels.iter_mut().zip(report.cancels) {
                *total += count;
            }
            *completed += report.completed;
            Ok(())
        })
        .unwrap();

    let (cancels, completed) = totals.into_inner().expect("not poisoned");
    println!(
        "completed {completed}, cancelled at {:?}",
        PHASES.iter().zip(cancels).collect::<Vec<_>>()
    );
    assert!(
        cancels.iter().all(|count| *count > 200),
        "every await point cancelled: {cancels:?}"
    );
}

/// Регрессия: без `OnDrop` отмена посреди операции оставляет CS опущенным —
/// следующая транзакция другого устройства идёт при двух опущенных CS.
#[test]
fn unguarded_cs_is_caught() {
    let hang_mid_read = Attempt {
        shape:  Shape::ReadReg { addr: 0x84, len: 4 },
        plan:   TxPlan {
            ops:   [
                OpPlan::CLEAN,
                OpPlan {
                    lead:    0,
                    outcome: Outcome::Hang,
                    after:   2,
                },
            ],
            flush: OpPlan::CLEAN,
        },
        budget: 8,
    };
    // A зависает и отменяется на 8-м опросе, затем ходит только B.
    let mut schedule = vec![2; 8];
    schedule.resize(40, 3);
    let scenario = Scenario {
        attempts: [vec![hang_mid_read], Vec::new()],
        schedule,
    };

    let guarded = simulate(Kind::Guarded, &scenario);
    assert_eq!(guarded.violations, Vec::<String>::new());
    assert_eq!(
        guarded.cancels[PHASES.iter().position(|p| *p == Phase::Hung).unwrap()],
        1
    );

    let unguarded = simulate(Kind::Unguarded, &scenario);
    let found = |needle: &str| unguarded.violations.iter().any(|v| v.contains(needle));
    assert!(
        found("CS still low") && found("CS 1 lowered while CS 0 is low"),
        "{:#?}",
        unguarded.violations
    );
}
