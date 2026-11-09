//! GICv3 ITS/LPI owner。冷配置使用可睡眠锁，硬 IRQ 只访问发布后稳定的属性表与 redistributor。
//! 硬件取得表指针后的命令/同步失败必须 fail-stop 保活全部 DMA 表，不能把超时当作已经撤销。
mod command;
mod table;
use super::device_tree::RangeValue;
use crate::execution::{
    cpu::{self, CpuId},
    sync::{IrqMutex, TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation},
};
use crate::platform::MessageInterruptAllocator;
use alloc::{sync::Arc, vec::Vec};
use command::Command;
use core::sync::atomic::{AtomicU8, Ordering};
use fallible_tree::FallibleMap;
use mmio::MmioBus;
use native_abi::{
    ERR_ALREADY_BOUND, ERR_INVALID_ARGS, ERR_NO_MEMORY, ERR_NO_RESOURCES, ERR_NOT_SUPPORTED, Status,
};
use spin::Once;
use table::{Frame, Table, allocate, physical};

const FIRST_LPI: u32 = 8192;
const COMMAND_BYTES: usize = 65536;
const TRANSLATER: usize = 0x10040;
// OWNER: 一个 DTB ITS；状态在首次 MSI 请求时创建，已发布的硬件页保活到系统结束。
static ITS: Once<Controller> = Once::new();
struct Controller {
    bus: MmioBus,
    physical: u64,
    typer: u64,
    state: TaskMutex<Option<State>>,
    properties: Once<Properties>,
    commands: Once<IrqMutex<Commands>>,
}
struct Properties {
    bytes: Frame,
    redistributors: Vec<Redistributor>,
    limit: u32,
    // OWNER: first LPI→device/event 的唯一软件索引；GIC IRQ → routes → commands，冷路径 state → routes。
    // prepare 在锁外分配，commit/remove 不分配；IRQ 不得取得可睡眠 state 锁。
    routes: IrqMutex<FallibleMap<u32, Arc<Device>>>,
}
struct Redistributor {
    cpu: CpuId,
    bus: MmioBus,
    target: u64,
    _pending: Frame,
}
struct Device {
    id: u32,
    _table: Frame,
    first: u32,
    count: u32,
    bits: u32,
}
struct State {
    device_table: Table,
    _collection_table: Option<Table>,
    allocated: Vec<u64>,
}

struct Commands {
    bytes: Frame,
    producer: usize,
}

/// 一组消息中断的唯一退休令牌；Interrupt 对象通过上层分配 owner 保活它。
pub(crate) struct MessageInterruptGroup {
    device: Arc<Device>,
    retirement: TaskMutexWaitPreparation,
}
impl MessageInterruptGroup {
    /// 分配设备的 event→LPI 映射；没有 ITS 时返回 NOT_SUPPORTED，发布前分配失败返回 NO_MEMORY。
    pub(crate) fn allocate(
        device: u32,
        count: u32,
        allocator: MessageInterruptAllocator,
    ) -> Result<Self, Status> {
        if count == 0 {
            return Err(ERR_INVALID_ARGS);
        }
        let controller = ITS.get().ok_or(ERR_NOT_SUPPORTED)?;
        let retirement = TaskMutexWaitPreparation::prepare().map_err(|_| ERR_NO_MEMORY)?;
        let mut slot = controller.state.lock().map_err(|_| ERR_NO_MEMORY)?;
        if slot.is_none() {
            *slot = Some(controller.activate(allocator)?);
        }
        let state = slot.as_mut().expect("ITS initialized");
        if controller
            .properties
            .wait()
            .routes
            .lock()
            .iter()
            .any(|(_, mapping)| mapping.id == device)
        {
            return Err(ERR_ALREADY_BOUND);
        }
        let event_capacity = count
            .max(2)
            .checked_next_power_of_two()
            .ok_or(ERR_NO_RESOURCES)?;
        let bits = event_capacity.trailing_zeros();
        if bits > ((controller.typer >> 8 & 31) + 1) as u32 {
            return Err(ERR_NO_RESOURCES);
        }
        let first = find_run(&state.allocated, count).ok_or(ERR_NO_RESOURCES)?;
        let table = allocate(
            allocator,
            event_capacity as usize * (((controller.typer >> 4) & 15) as usize + 1),
        )?;
        let mapping = Arc::try_new(Device {
            id: device,
            _table: table,
            first,
            count,
            bits,
        })
        .map_err(|_| ERR_NO_MEMORY)?;
        let prepared =
            FallibleMap::try_prepare(first, mapping.clone()).map_err(|_| ERR_NO_MEMORY)?;
        state.device_table.ensure(allocator, device)?;
        let boot = controller.properties.wait().redistributor(cpu::boot_id());
        controller.command(Command::map_device(
            device,
            physical(&mapping._table),
            bits,
            true,
        ));
        for event in 0..count {
            controller.command(Command::map_interrupt(
                device,
                event,
                first + event,
                collection(cpu::boot_id()),
            ));
        }
        controller.command(Command::synchronize(boot.target));
        controller.commit();
        for vector in first..first + count {
            set_bit(&mut state.allocated, vector, true);
        }
        controller
            .properties
            .wait()
            .routes
            .lock()
            .commit_vacant(prepared);
        Ok(Self {
            device: mapping,
            retirement,
        })
    }
    pub(crate) fn count(&self) -> u32 {
        self.device.count
    }
    pub(crate) fn vector(&self, index: u32) -> Option<u32> {
        (index < self.device.count).then(|| self.device.first + index)
    }
    /// 在修改设备消息前准备目标；冷锁串行准备/提交与整组退休，错误不改变旧路由。
    /// # Errors
    /// 非法 index/CPU 返回 INVALID_ARGS；冷锁等待准备失败返回 NO_MEMORY。
    pub(crate) fn prepare_affinity(
        &self,
        index: u32,
        target: CpuId,
    ) -> Result<MessageInterruptRoute<'_>, Status> {
        if index >= self.count() || !cpu::online().contains(target) {
            return Err(ERR_INVALID_ARGS);
        }
        let controller = ITS.wait();
        controller.properties.wait().redistributor(target);
        let state = controller.state.lock().map_err(|_| ERR_NO_MEMORY)?;
        Ok(MessageInterruptRoute {
            controller,
            _state: state,
            device: self.device.id,
            event: index,
            target,
        })
    }
}

/// 已准备但尚未修改硬件的消息迁移；析构只释放冷锁，取消不改变旧路由。
pub(crate) struct MessageInterruptRoute<'a> {
    controller: &'a Controller,
    _state: TaskMutexGuard<'a, Option<State>>,
    device: u32,
    event: u32,
    target: CpuId,
}
impl MessageInterruptRoute<'_> {
    /// 新路由的物理消息目标；上层须先完成 BTI 映射再屏蔽/改写设备表项。
    pub(crate) fn message(&self) -> (u64, u32) {
        (self.controller.physical + TRANSLATER as u64, self.event)
    }
    /// 源表项已屏蔽且新消息值写回后提交一次；没有可恢复失败，硬件超时 fail-stop 保活表。
    /// 调用者在恢复源 mask/ack 状态后才释放本令牌，避免下一次配置抢入未完成的源事务。
    pub(crate) fn commit(&mut self, _acknowledge_local: impl Fn()) {
        // MOVI 的 SYNC 覆盖旧/新 collection，不能只同步新目标；不复制另一份 CPU 路由状态。
        self.controller.synchronize_command(Command::move_interrupt(
            self.device,
            self.event,
            collection(self.target),
        ));
    }
}
impl Drop for MessageInterruptGroup {
    fn drop(&mut self) {
        let controller = ITS.wait();
        let mut slot = controller.state.lock_prepared(&mut self.retirement);
        let state = slot.as_mut().expect("ITS active");
        let device = &self.device;
        let (first, count, bits) = (device.first, device.count, device.bits);
        // 1. 全部 Interrupt 已 unbind；先撤销 event 与 device，SYNC 完成后才允许 ID/ITT 重用。
        for event in 0..count {
            controller.command(Command::discard(self.device.id, event));
        }
        for redistributor in &controller.properties.wait().redistributors {
            controller.command(Command::synchronize(redistributor.target));
        }
        controller.command(Command::map_device(self.device.id, 0, bits, false));
        for redistributor in &controller.properties.wait().redistributors {
            controller.command(Command::synchronize(redistributor.target));
        }
        controller.commit();
        for vector in first..first + count {
            set_bit(&mut state.allocated, vector, false);
        }
        drop(controller.properties.wait().routes.lock().remove(&first));
    }
}

pub(super) fn initialize(info: Option<RangeValue>) {
    let Some(info) = info else {
        return;
    };
    assert!(info.size >= 0x20000, "ITS register frame incomplete");
    let bus = MmioBus::new(crate::arch::mmu::physical_to_virtual(info.start), info.size)
        .expect("ITS MMIO range");
    let typer = bus.read_u64(8).expect("ITS TYPER");
    assert_ne!(typer & 1, 0, "ITS lacks physical LPIs");
    ITS.call_once(|| Controller {
        bus,
        physical: info.start as u64,
        typer,
        state: TaskMutex::new(None),
        properties: Once::new(),
        commands: Once::new(),
    });
}
impl Controller {
    /// 独立 IRQ-safe command owner；不取得可睡眠 state 锁，也不分配。
    fn synchronize_command(&self, command: Command) {
        let mut commands = self.commands.wait().lock();
        commands.command(self.bus, command);
        for target in &self.properties.wait().redistributors {
            commands.command(self.bus, Command::synchronize(target.target));
        }
        commands.commit(self.bus);
    }
    fn command(&self, command: Command) {
        self.commands.wait().lock().command(self.bus, command);
    }
    fn commit(&self) {
        self.commands.wait().lock().commit(self.bus);
    }
    fn activate(&self, allocator: MessageInterruptAllocator) -> Result<State, Status> {
        let (id_bits, redistributors) =
            super::gicv3::lpi_redistributors(self.typer & (1 << 19) != 0)?;
        let limit = 1u32 << id_bits.min(16);
        let mut properties = allocate(allocator, (limit as usize - FIRST_LPI as usize).max(4096))?;
        properties.bytes_mut().fill(0xa2); // priority 0xa0、Group 1；尚不启用任何 LPI。
        let mut targets = Vec::new();
        targets
            .try_reserve_exact(redistributors.len())
            .map_err(|_| ERR_NO_MEMORY)?;
        for source in redistributors {
            targets.push(Redistributor {
                cpu: source.cpu,
                bus: source.bus,
                target: source.target,
                _pending: allocate(allocator, 65536)?,
            });
        }
        let commands = allocate(allocator, COMMAND_BYTES)?;
        let mut allocated = Vec::new();
        allocated
            .try_reserve_exact((limit - FIRST_LPI) as usize / 64)
            .map_err(|_| ERR_NO_MEMORY)?;
        allocated.resize((limit - FIRST_LPI) as usize / 64, 0);
        // 2. ITS/RD 停止后才重配 BASER。所有 fallible backing/metadata 先准备，再交出硬件所有权。
        self.bus.write_u32(0, 0).expect("ITS disable");
        wait_until(
            || self.bus.read_u32(0).expect("ITS CTLR") & (1 << 31) != 0,
            "ITS quiescence",
        );
        let mut device_table = None;
        let mut collection_table = None;
        // GITS_BASER0..7 是八个架构寄存器，不是固定 CPU 数。
        for register in (0x100..0x140).step_by(8) {
            let value = self.bus.read_u64(register).expect("ITS BASER");
            match value >> 56 & 7 {
                1 if device_table.is_none() => {
                    device_table = Some(Table::prepare(
                        allocator,
                        self.bus,
                        register,
                        (self.typer >> 13 & 31) as u32 + 1,
                        true,
                    )?)
                }
                4 if collection_table.is_none() => {
                    collection_table = Some(Table::prepare(
                        allocator,
                        self.bus,
                        register,
                        cpu::possible()
                            .iter()
                            .count()
                            .max(2)
                            .next_power_of_two()
                            .trailing_zeros(),
                        false,
                    )?)
                }
                0 | 1 | 4 => {}
                _ => return Err(ERR_NOT_SUPPORTED),
            }
        }
        let device_table = device_table.ok_or(ERR_NOT_SUPPORTED)?;
        for target in &targets {
            target
                .bus
                .write_u32(0, target.bus.read_u32(0).expect("GICR CTLR") & !1)
                .expect("GICR disable LPIs");
            wait_until(
                || target.bus.read_u32(0).expect("GICR CTLR") & ((1 << 3) | 1) == 0,
                "LPI disable",
            );
            target
                .bus
                .write_u64(
                    0x70,
                    physical(&properties) | 1 << 10 | 7 << 7 | u64::from(id_bits.min(16) - 1),
                )
                .expect("GICR PROPBASER");
            target
                .bus
                .write_u64(
                    0x78,
                    physical(&target._pending) | 1 << 10 | 7 << 7 | 1 << 62,
                )
                .expect("GICR PENDBASER");
            for offset in [0x70, 0x78] {
                assert_eq!(
                    target.bus.read_u64(offset).expect("GICR BASER readback") & (3 << 10 | 7 << 7),
                    1 << 10 | 7 << 7,
                    "LPIs require coherent tables"
                );
            }
        }
        device_table.publish(self.bus);
        if let Some(table) = &collection_table {
            table.publish(self.bus);
        }
        self.bus
            .write_u64(
                0x80,
                physical(&commands)
                    | (1 << 63)
                    | 7 << 59
                    | 1 << 10
                    | (COMMAND_BYTES as u64 / 4096 - 1),
            )
            .expect("ITS CBASER");
        let cbaser = self.bus.read_u64(0x80).expect("ITS CBASER readback");
        assert_eq!(
            cbaser & (1 << 63 | 7 << 59 | 3 << 10),
            1 << 63 | 7 << 59 | 1 << 10,
            "ITS command memory must be coherent"
        );
        self.bus.write_u64(0x88, 0).expect("ITS CWRITER");
        self.commands.call_once(|| {
            IrqMutex::new(Commands {
                bytes: commands,
                producer: 0,
            })
        });
        let properties = self.properties.call_once(|| Properties {
            bytes: properties,
            redistributors: targets,
            limit,
            routes: IrqMutex::new(FallibleMap::new()),
        });
        crate::arch::dma::complete_table_writes();
        for target in &properties.redistributors {
            target
                .bus
                .write_u32(0, target.bus.read_u32(0).expect("GICR CTLR") | 1)
                .expect("GICR enable LPIs");
        }
        self.bus.write_u32(0, 1).expect("ITS enable");
        let state = State {
            device_table,
            _collection_table: collection_table,
            allocated,
        };
        for target in &properties.redistributors {
            self.command(Command::map_collection(
                collection(target.cpu),
                target.target,
            ));
            self.command(Command::synchronize(target.target));
        }
        self.commit();
        Ok(state)
    }
}
impl Properties {
    fn event(&self, vector: u32) -> (u32, u32) {
        let routes = self.routes.lock();
        let (&first, device) = routes.floor(&vector).expect("bound LPI has a route");
        assert!(
            vector - first < device.count,
            "bound LPI belongs to a live group"
        );
        (device.id, vector - first)
    }
    fn redistributor(&self, cpu: CpuId) -> &Redistributor {
        self.redistributors
            .iter()
            .find(|target| target.cpu == cpu)
            .expect("CPU has ITS collection")
    }
}
impl Commands {
    fn command(&mut self, bus: MmioBus, command: Command) {
        let next = (self.producer + 32) % COMMAND_BYTES;
        if next == (bus.read_u64(0x90).expect("ITS CREADR") as usize & (COMMAND_BYTES - 1)) {
            self.commit(bus);
        }
        for (index, word) in command.0.iter().enumerate() {
            self.bytes.bytes_mut()[self.producer + index * 8..self.producer + index * 8 + 8]
                .copy_from_slice(&word.to_le_bytes());
        }
        self.producer = next;
        // bounded publication；ring 即使 count>capacity 也不能覆盖未消费的命令。
        crate::arch::dma::complete_table_writes();
        bus.write_u64(0x88, self.producer as u64)
            .expect("ITS command publication");
    }
    fn commit(&self, bus: MmioBus) {
        wait_until(
            || bus.read_u64(0x90).expect("ITS CREADR") == self.producer as u64,
            "ITS command completion",
        );
    }
}
fn find_run(allocated: &[u64], count: u32) -> Option<u32> {
    let mut run = 0;
    for bit in 0..allocated.len() as u32 * 64 {
        run = if allocated[bit as usize / 64] & (1 << (bit % 64)) == 0 {
            run + 1
        } else {
            0
        };
        if run == count {
            return Some(FIRST_LPI + bit + 1 - count);
        }
    }
    None
}
fn set_bit(allocated: &mut [u64], vector: u32, set: bool) {
    let index = vector - FIRST_LPI;
    let mask = 1 << (index % 64);
    let word = &mut allocated[index as usize / 64];
    if set {
        *word |= mask;
    } else {
        *word &= !mask;
    }
}
fn collection(cpu: CpuId) -> u16 {
    u16::try_from(cpu.index()).expect("ITS collection width covers logical CPUs")
}
fn wait_until(mut done: impl FnMut() -> bool, label: &str) {
    let deadline =
        crate::arch::time::counter().saturating_add(crate::arch::time::counter_frequency());
    while !done() {
        assert!(
            crate::arch::time::counter() < deadline,
            "{label} timed out; retain published ITS tables"
        );
        core::hint::spin_loop();
    }
}
/// 在 GIC IRQ-safe 锁内改变单个属性，所有 redistributor 缓存均失效，覆盖并发路由迁移的源/目标。
pub(super) fn set_enabled(vector: u32, enabled: bool) -> bool {
    let Some(properties) = ITS.get().and_then(|its| its.properties.get()) else {
        return false;
    };
    if !(FIRST_LPI..properties.limit).contains(&vector) {
        return false;
    }
    // SAFETY: 属性页由 static owner 保活；IRQ/GIC 锁串行化 CPU 修改，控制器只读该字节。
    let byte = unsafe {
        &*(crate::arch::mmu::physical_to_virtual(physical(&properties.bytes) as usize)
            as *mut AtomicU8)
            .add((vector - FIRST_LPI) as usize)
    };
    if enabled {
        byte.fetch_or(1, Ordering::Relaxed);
    } else {
        byte.fetch_and(!1, Ordering::Relaxed);
    }
    crate::arch::dma::complete_table_writes();
    // 标准 ITS INV 只失效此 event，不清空其他队列的缓存；不依赖可选的 DirectLPIS。
    // 源/目标路由迁移可能并发，因此 SYNC 覆盖全部 collection，不另存一份 CPU 路由状态。
    let (device, event) = properties.event(vector);
    ITS.wait()
        .synchronize_command(Command::invalidate(device, event));
    true
}
pub(super) fn contains(vector: u32) -> bool {
    ITS.get()
        .and_then(|its| its.properties.get())
        .is_some_and(|properties| (FIRST_LPI..properties.limit).contains(&vector))
}
/// 最后一个绑定撤销后清除硬件 pending，再允许组内槽重用；调用方持 GIC 锁。
pub(super) fn clear(vector: u32) {
    if !contains(vector) {
        return;
    }
    let controller = ITS.wait();
    let (device, event) = controller.properties.wait().event(vector);
    controller.synchronize_command(Command::clear(device, event));
}
