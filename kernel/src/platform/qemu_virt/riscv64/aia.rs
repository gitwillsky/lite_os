//! APLIC/IMSIC 的唯一路由 owner。冷锁串行资源事务，IRQ 锁仅保护已准备的路由与本地 CSR 请求。
mod aplic;
mod msi;
pub(crate) use msi::{MessageInterruptGroup, MessageInterruptRoute};

use super::{device_tree::AiaInfo, discovery};
use crate::arch::interrupt;
use crate::execution::{
    cpu::{self, CpuId, CpuSet},
    interrupt::{InterruptError, InterruptHandler},
    sync::{IrqMutex, TaskMutex, TaskMutexWaitPreparation},
};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use fallible_tree::FallibleMap;
use mmio::MmioBus;
use native_abi::{ERR_ALREADY_BOUND, ERR_INVALID_ARGS, ERR_NO_MEMORY, ERR_NO_RESOURCES, Status};
use spin::Once;

const MESSAGE_BASE: u32 = 65536;
const SYNC_ID: usize = 1;
// OWNER: 唯一已发现的 S-mode AIA；全部索引与 MMIO 窗口保留至机器停止。
static AIA: Once<Controller> = Once::new();
struct Controller {
    aplic: aplic::Aplic,
    identities: u32,
    files: Vec<File>,
    // OWNER: 资源准备/提交/退休的 task-only 事务；硬 IRQ 不获取此锁。
    cold: TaskMutex<Cold>,
    // OWNER: 所有 vector→handler/target 与 CPU EIID→vector 的唯一索引；不得在本锁内分配或等待。
    routes: IrqMutex<Routes>,
}
struct File {
    address: u64,
    bus: MmioBus,
    hart_index: u32,
    // OWNER: 冷事务发布 CSR 请求代号，目标 CPU 确认；没有 acquire/release 会提前复用仍 pending 的 EIID。
    request: AtomicU64,
    completion: AtomicU64,
}
struct Cold {
    logical: Vec<u64>,
    devices: FallibleMap<u32, ()>,
    generation: u64,
}
struct Routes {
    vectors: FallibleMap<u32, Route>,
    locals: Vec<Local>,
}
struct Local {
    // 0 为 free；其余值是稳定逻辑 vector，不是本 CPU 的 EIID。迁移临时保留旧/新槽指向同一路由。
    slots: Vec<u32>,
    // OWNER: 待本 CPU 投影到 CSR 的 EIID；IRQ 上只清除预分配位图，不分配队列。
    dirty: Vec<u64>,
    // GENMSI 标准 fence 尚未到达；只允许一个冷事务使用 ID 1，不能用延时猜测旧消息已经排空。
    marker: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    cpu: CpuId,
    id: u32,
}
struct Route {
    target: Option<Target>,
    handler: Option<Arc<dyn InterruptHandler>>,
    kind: Kind,
}
enum Kind {
    Message,
    Wire { level: bool, state: WireState },
}
enum WireState {
    Armed,
    Masked,
    // 冷迁移期间实际 APLIC 源已 mask；IRQ/ack 仅更新恢复意图，不能提前放行旧 target。
    Updating { unmask: bool },
}

/// 原生 Interrupt 关闭前已准备的冷锁等待存储；退休不得因 OOM 跳过硬件确认。
pub(crate) struct InterruptRetirement(TaskMutexWaitPreparation);
impl InterruptRetirement {
    pub(crate) fn prepare() -> Result<Self, Status> {
        TaskMutexWaitPreparation::prepare()
            .map(Self)
            .map_err(|_| ERR_NO_MEMORY)
    }
}

pub(super) fn initialize() {
    let facts: &AiaInfo = &discovery::info().aia;
    let mut files = Vec::new();
    let mut locals = Vec::new();
    files
        .try_reserve_exact(cpu::count())
        .expect("IMSIC file table");
    locals
        .try_reserve_exact(cpu::count())
        .expect("IMSIC local table");
    for cpu in cpu::possible().iter() {
        let fact = facts
            .files
            .iter()
            .find(|file| file.hardware_cpu == cpu::hardware_id(cpu).raw())
            .expect("CPU has IMSIC file");
        files.push(File {
            address: fact.address as u64,
            bus: MmioBus::new(crate::arch::mmu::physical_to_virtual(fact.address), 4096)
                .expect("IMSIC MMIO window"),
            hart_index: fact.hart_index,
            request: AtomicU64::new(0),
            completion: AtomicU64::new(0),
        });
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(facts.identities as usize + 1)
            .expect("IMSIC slots");
        slots.resize(facts.identities as usize + 1, 0);
        let mut dirty = Vec::new();
        dirty
            .try_reserve_exact((facts.identities as usize + 1).div_ceil(64))
            .expect("IMSIC dirty words");
        dirty.resize((facts.identities as usize + 1).div_ceil(64), 0);
        locals.push(Local {
            slots,
            dirty,
            marker: false,
        });
    }
    let mut logical = Vec::new();
    let capacity = (facts.identities as usize - 1) * files.len();
    logical
        .try_reserve_exact(capacity.div_ceil(64))
        .expect("MSI logical allocation map");
    logical.resize(capacity.div_ceil(64), 0);
    let assembled = Controller {
        aplic: aplic::Aplic::new(facts.aplic.clone(), facts.sources),
        identities: facts.identities,
        files,
        cold: TaskMutex::new(Cold {
            logical,
            devices: FallibleMap::new(),
            generation: 0,
        }),
        routes: IrqMutex::new(Routes {
            vectors: FallibleMap::new(),
            locals,
        }),
    };
    assert!(AIA.get().is_none(), "AIA initialized twice");
    AIA.call_once(|| assembled);
    initialize_local();
    controller().aplic.start();
}
pub(super) fn initialize_local() {
    // 本 CPU 尚未上线，不能已有远端请求；CSR mechanism 只在 arch 内编码。
    interrupt::initialize_messages(controller().identities);
}
fn controller() -> &'static Controller {
    AIA.wait()
}

impl Routes {
    fn reserve(&mut self, vector: u32, cpu: CpuId) -> Result<Target, Status> {
        let (index, slot) = self.locals[cpu.index()]
            .slots
            .iter_mut()
            .enumerate()
            .skip(2)
            .find(|(_, vector)| **vector == 0)
            .ok_or(ERR_NO_RESOURCES)?;
        *slot = vector;
        Ok(Target {
            cpu,
            id: index as u32,
        })
    }
    fn dirty(&mut self, target: Target) {
        self.locals[target.cpu.index()].dirty[target.id as usize / 64] |= 1u64 << (target.id % 64);
    }
}
impl Controller {
    fn signal(&self, target: Target) {
        self.files[target.cpu.index()]
            .bus
            .write_u32(0, target.id)
            .expect("IMSIC message write");
    }
    fn begin(&self, cold: &mut Cold, targets: CpuSet) -> u64 {
        cold.generation = cold
            .generation
            .checked_add(1)
            .expect("AIA generation exhausted");
        let generation = cold.generation;
        for cpu in targets.iter() {
            self.files[cpu.index()]
                .request
                .store(generation, Ordering::Release);
        }
        let mut remote = targets;
        remote.remove(cpu::current_id());
        if !remote.is_empty() {
            super::firmware::send_ipi(remote).expect("AIA CSR IPI failed; retain routes");
        }
        generation
    }
    fn wait(&self, targets: CpuSet, generation: u64, acknowledge_local: impl Fn()) {
        aplic::wait(
            || {
                targets.iter().all(|cpu| {
                    self.files[cpu.index()].completion.load(Ordering::Acquire) >= generation
                })
            },
            || {
                complete_pending();
                acknowledge_local();
            },
        );
    }
    fn synchronize(&self, cold: &mut Cold, targets: CpuSet, acknowledge_local: impl Fn()) {
        let generation = self.begin(cold, targets);
        self.wait(targets, generation, acknowledge_local);
    }
    fn fence_aplic(&self, cold: &mut Cold, target: Target, acknowledge_local: impl Fn()) {
        let targets = CpuSet::singleton(target.cpu);
        {
            let mut routes = self.routes.lock();
            let local = &mut routes.locals[target.cpu.index()];
            assert!(!local.marker, "APLIC marker transactions are serialized");
            local.dirty[0] |= 1 << SYNC_ID;
        }
        // 1. 在目标 CPU 清除专用 pending 位并确认，然后才生成新 marker。
        self.synchronize(cold, targets, &acknowledge_local);
        {
            let mut routes = self.routes.lock();
            let local = &mut routes.locals[target.cpu.index()];
            local.marker = true;
            local.dirty[0] |= 1 << SYNC_ID;
        }
        let generation = self.begin(cold, targets);
        self.aplic
            .marker(self.files[target.cpu.index()].hart_index, || {
                complete_pending();
                acknowledge_local();
            });
        // 2. 目标文件收到 marker 后才确认；AIA 保证此前发往同一文件的 APLIC MSI 都已到达。
        self.wait(targets, generation, acknowledge_local);
    }
    fn move_route(&self, cold: &mut Cold, vector: u32, next: Target, acknowledge_local: impl Fn()) {
        let previous = {
            let mut routes = self.routes.lock();
            let route = routes
                .vectors
                .get_mut(&vector)
                .expect("prepared AIA vector");
            let previous = route.target.replace(next);
            routes.dirty(next);
            if let Some(previous) = previous {
                routes.dirty(previous);
            }
            previous
        };
        let mut targets = CpuSet::singleton(next.cpu);
        if let Some(previous) = previous {
            targets.insert(previous.cpu);
        }
        self.synchronize(cold, targets, acknowledge_local);
    }
}

/// 无分配地消费当前 CPU 的 CSR 请求；IRQ 或关中断的协作等待都可调用。
pub(crate) fn complete_pending() {
    let Some(controller) = AIA.get() else {
        return;
    };
    let current = cpu::current_id();
    let file = &controller.files[current.index()];
    let requested = file.request.load(Ordering::Acquire);
    if requested <= file.completion.load(Ordering::Relaxed) {
        return;
    }
    let mut routes = controller.routes.lock();
    for word in 0..routes.locals[current.index()].dirty.len() {
        let mut dirty = core::mem::take(&mut routes.locals[current.index()].dirty[word]);
        while dirty != 0 {
            let bit = dirty.trailing_zeros();
            dirty &= dirty - 1;
            let id = word as u32 * 64 + bit;
            if id == SYNC_ID as u32 {
                let waiting = routes.locals[current.index()].marker;
                interrupt::enable_message(id, waiting);
                if !waiting {
                    interrupt::take_message_pending(id);
                }
                continue;
            }
            let vector = routes.locals[current.index()].slots[id as usize];
            assert_ne!(vector, 0, "dirty IMSIC slot has a route");
            let route = routes
                .vectors
                .get(&vector)
                .expect("IMSIC slot owns a route");
            let target = route.target;
            let local_target = Some(Target { cpu: current, id });
            let eligible = target == local_target && route.handler.is_some();
            interrupt::enable_message(id, eligible);
            if !eligible {
                let pending = interrupt::take_message_pending(id);
                if target != local_target {
                    if pending && route.handler.is_some() {
                        controller.signal(target.expect("bound route has target"));
                    }
                    routes.locals[current.index()].slots[id as usize] = 0;
                }
            }
        }
    }
    let local = &mut routes.locals[current.index()];
    if local.marker && interrupt::take_message_pending(SYNC_ID as u32) {
        local.marker = false;
        interrupt::enable_message(SYNC_ID as u32, false);
    }
    if !local.marker {
        file.completion.fetch_max(requested, Ordering::Release);
    }
}

pub(crate) fn device_interrupt_range() -> core::ops::Range<u32> {
    1..controller().aplic.sources + 1
}

pub(crate) fn bind_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    level: bool,
    acknowledge_local: impl Fn(),
) -> Result<(), Status> {
    let controller = controller();
    if !device_interrupt_range().contains(&vector) {
        return Err(ERR_INVALID_ARGS);
    }
    let mut cold = controller.cold.lock().map_err(|_| ERR_NO_MEMORY)?;
    let prepared = FallibleMap::try_prepare(
        vector,
        Route {
            target: None,
            handler: Some(handler),
            kind: Kind::Wire {
                level,
                state: WireState::Masked,
            },
        },
    )
    .map_err(|_| ERR_NO_MEMORY)?;
    let target = {
        let mut routes = controller.routes.lock();
        if routes.vectors.contains_key(&vector) {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        let target = routes.reserve(vector, cpu::boot_id())?;
        routes.vectors.commit_vacant(prepared);
        routes
            .vectors
            .get_mut(&vector)
            .expect("new wired route")
            .target = Some(target);
        routes.dirty(target);
        target
    };
    controller.aplic.configure(vector, Some(level));
    controller.aplic.target(
        vector,
        controller.files[target.cpu.index()].hart_index,
        target.id,
    );
    controller.synchronize(&mut cold, CpuSet::singleton(target.cpu), acknowledge_local);
    unmask_interrupt(vector).expect("published wired AIA route can arm");
    Ok(())
}
pub(crate) fn bind_message_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    acknowledge_local: impl Fn(),
) -> Result<(), Status> {
    let controller = controller();
    let mut cold = controller.cold.lock().map_err(|_| ERR_NO_MEMORY)?;
    let target = {
        let mut routes = controller.routes.lock();
        let route = routes.vectors.get_mut(&vector).ok_or(ERR_INVALID_ARGS)?;
        if !matches!(route.kind, Kind::Message) {
            return Err(ERR_INVALID_ARGS);
        }
        if route.handler.is_some() {
            return Err(ERR_ALREADY_BOUND);
        }
        let target = route.target.ok_or(ERR_INVALID_ARGS)?;
        route.handler = Some(handler);
        routes.dirty(target);
        target
    };
    controller.synchronize(&mut cold, CpuSet::singleton(target.cpu), acknowledge_local);
    Ok(())
}
pub(crate) fn unmask_interrupt(vector: u32) -> Result<(), InterruptError> {
    let controller = controller();
    let mut routes = controller.routes.lock();
    let route = routes
        .vectors
        .get_mut(&vector)
        .ok_or(InterruptError::HandlerNotSet)?;
    let Kind::Wire { level, state } = &mut route.kind else {
        return Err(InterruptError::InvalidVector);
    };
    match state {
        WireState::Updating { unmask } => *unmask = true,
        state => {
            controller.aplic.enable(vector, true);
            if *level {
                controller.aplic.retrigger(vector);
            }
            *state = WireState::Armed;
        }
    }
    Ok(())
}
pub(crate) fn set_interrupt_affinity(
    vector: u32,
    cpu: CpuId,
    acknowledge_local: impl Fn(),
) -> Result<(), Status> {
    let controller = controller();
    if !cpu::online().contains(cpu) {
        return Err(ERR_INVALID_ARGS);
    }
    let mut cold = controller.cold.lock().map_err(|_| ERR_NO_MEMORY)?;
    let (previous, next) = {
        let mut routes = controller.routes.lock();
        let route = routes
            .vectors
            .get(&vector)
            .ok_or(native_abi::ERR_BAD_STATE)?;
        if !matches!(route.kind, Kind::Wire { .. }) {
            return Err(ERR_INVALID_ARGS);
        }
        let previous = route.target.expect("wired target");
        if previous.cpu == cpu {
            return Ok(());
        }
        let next = routes.reserve(vector, cpu)?;
        let route = routes.vectors.get_mut(&vector).expect("wired route");
        let Kind::Wire { state, .. } = &mut route.kind else {
            unreachable!()
        };
        let unmask = matches!(state, WireState::Armed);
        controller.aplic.enable(vector, false);
        *state = WireState::Updating { unmask };
        (previous, next)
    };
    controller.fence_aplic(&mut cold, previous, &acknowledge_local);
    controller.aplic.target(
        vector,
        controller.files[next.cpu.index()].hart_index,
        next.id,
    );
    controller.move_route(&mut cold, vector, next, acknowledge_local);
    let mut routes = controller.routes.lock();
    let route = routes
        .vectors
        .get_mut(&vector)
        .expect("live wired migration");
    let Kind::Wire { level, state } = &mut route.kind else {
        unreachable!()
    };
    let WireState::Updating { unmask } = *state else {
        panic!("serialized APLIC migration");
    };
    if unmask {
        controller.aplic.enable(vector, true);
        if *level {
            controller.aplic.retrigger(vector);
        }
        *state = WireState::Armed;
    } else {
        *state = WireState::Masked;
    }
    Ok(())
}
pub(crate) fn unbind_interrupt(
    vector: u32,
    retirement: &mut InterruptRetirement,
    acknowledge_local: impl Fn(),
) -> Option<Arc<dyn InterruptHandler>> {
    let controller = controller();
    let mut cold = controller.cold.lock_prepared(&mut retirement.0);
    let (target, wired) = {
        let mut routes = controller.routes.lock();
        let route = routes.vectors.get_mut(&vector)?;
        let target = route.target.expect("bound AIA target");
        let wired = matches!(route.kind, Kind::Wire { .. });
        if wired {
            controller.aplic.enable(vector, false);
            controller.aplic.configure(vector, None);
        }
        (target, wired)
    };
    if wired {
        controller.fence_aplic(&mut cold, target, &acknowledge_local);
    }
    let handler = {
        let mut routes = controller.routes.lock();
        let route = routes.vectors.get_mut(&vector).expect("retiring route");
        route.target = None;
        let handler = route.handler.take();
        routes.dirty(target);
        handler
    };
    controller.synchronize(&mut cold, CpuSet::singleton(target.cpu), acknowledge_local);
    if wired {
        drop(controller.routes.lock().vectors.remove(&vector));
        controller.aplic.target(vector, 0, 0);
    }
    handler
}

/// 有界处理当前 CPU 的 eligible 消息；STOPEI claim 自带 EIP 清除，不再写第二次 EOI。
pub(crate) fn handle_external_interrupt() {
    let controller = controller();
    for _ in 0..64 {
        let id = interrupt::claim_message();
        if id == 0 {
            break;
        }
        let current = cpu::current_id();
        let mut routes = controller.routes.lock();
        if id == SYNC_ID as u32 {
            let local = &mut routes.locals[current.index()];
            assert!(local.marker, "unexpected APLIC fence marker");
            local.marker = false;
            interrupt::enable_message(id, false);
            drop(routes);
            complete_pending();
            continue;
        }
        let vector = routes.locals[current.index()].slots[id as usize];
        let route = routes
            .vectors
            .get_mut(&vector)
            .expect("claimed IMSIC identity has an owner");
        let Some(target) = route.target else {
            // 关闭已停止源，但此 CPU 可能先 claim 了旧 pending；CSR 同步完成前保留路由吸收它。
            interrupt::enable_message(id, false);
            continue;
        };
        if target != (Target { cpu: current, id }) {
            controller.signal(target);
            continue;
        }
        let Some(handler) = route.handler.as_ref() else {
            interrupt::enable_message(id, false);
            continue;
        };
        if handler.masks_until_ack() {
            let Kind::Wire { state, .. } = &mut route.kind else {
                panic!("MSI-X masks at its source");
            };
            match state {
                WireState::Updating { unmask } => *unmask = false,
                state => {
                    controller.aplic.enable(vector, false);
                    *state = WireState::Masked;
                }
            }
        }
        let result = handler.handle_interrupt(vector);
        if matches!(route.kind, Kind::Wire { level: true, .. }) {
            controller.aplic.retrigger(vector);
        }
        if let Err(error) = result {
            crate::error!("AIA handler {vector} failed: {error}");
        }
    }
}
