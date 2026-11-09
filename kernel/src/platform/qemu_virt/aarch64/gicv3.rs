//! QEMU `virt` GICv3 distributor、redistributor 与 ICC system-register owner。

use alloc::sync::Arc;
use core::arch::asm;
use mmio::MmioBus;

use spin::Once;

use crate::{
    arch::interrupt::SOFTWARE_SGI,
    execution::cpu::{self, CpuSet},
    execution::interrupt::InterruptError,
    execution::interrupt::InterruptHandler,
    execution::interrupt::InterruptVector,
    execution::sync::IrqMutex,
};
use fallible_tree::FallibleMap;

use super::{super::ClaimedInterrupt, device_tree::GicV3Info};

const TIMER_PPI: u32 = 27;
const SPURIOUS_INTERRUPT_MIN: u32 = 1020;
const REDISTRIBUTOR_STRIDE: usize = 0x20_000;

const GICD_CTLR: usize = 0x0000;
const GICD_TYPER: usize = 0x0004;
const GICD_IGROUPR: usize = 0x0080;
const GICD_ISENABLER: usize = 0x0100;
const GICD_ICENABLER: usize = 0x0180;
const GICD_ICACTIVER: usize = 0x0380;
const GICD_IPRIORITYR: usize = 0x0400;
const GICD_ICFGR: usize = 0x0c00;
const GICD_IROUTER: usize = 0x6000;
const GICD_CTLR_RWP: u32 = 1 << 31;
const GICD_CTLR_ARE_NS: u32 = 1 << 5;
const GICD_CTLR_ENABLE_G1_NS: u32 = 1 << 1;
const GICD_TYPER_RSS: u32 = 1 << 26;
const ICC_CTLR_RSS: u64 = 1 << 18;

const GICR_CTLR: usize = 0x0000;
const GICR_WAKER: usize = 0x0014;
const GICR_CTLR_RWP: u32 = 1 << 3;
const GICR_TYPER: usize = 0x0008;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_SGI_BASE: usize = 0x10_000;

// OWNER: platform owns one concrete GICv3 adapter; no runtime architecture dispatch is used.
static GIC: Once<IrqMutex<GicV3>> = Once::new();

/// GIC 解绑只需要 IRQ-safe 控制器锁；没有需要预分配的 task waiter。
pub(crate) struct InterruptRetirement;
impl InterruptRetirement {
    pub(crate) fn prepare() -> Result<Self, native_abi::Status> {
        Ok(Self)
    }
}

pub(crate) struct GicV3 {
    distributor: MmioRange,
    redistributor: MmioRange,
    redistributor_physical: usize,
    max_interrupt: u32,
    requires_range_selector: bool,
    possible_cpus: CpuSet,
    handlers: FallibleMap<InterruptVector, Arc<dyn InterruptHandler>>,
}

/// GIC frame 的 MMIO window；越界或未对齐访问违反 DTB 不变量，fail-stop。
#[derive(Clone, Copy)]
struct MmioRange(MmioBus);

impl MmioRange {
    fn new(base: usize, size: usize) -> Result<Self, InterruptError> {
        if size < core::mem::size_of::<u64>() {
            return Err(InterruptError::InvalidVector);
        }
        MmioBus::new(base, size)
            .map(Self)
            .map_err(|_| InterruptError::InvalidVector)
    }

    fn read32(self, offset: usize) -> u32 {
        self.0
            .read_u32(offset)
            .expect("GIC register outside DTB range")
    }

    fn write32(self, offset: usize, value: u32) {
        self.0
            .write_u32(offset, value)
            .expect("GIC register outside DTB range");
    }

    fn read64(self, offset: usize) -> u64 {
        self.0
            .read_u64(offset)
            .expect("GIC 64-bit register outside DTB range or unaligned")
    }

    fn write64(self, offset: usize, value: u64) {
        self.0
            .write_u64(offset, value)
            .expect("GIC 64-bit register outside DTB range or unaligned");
    }

    fn subrange(self, offset: usize, size: usize) -> Self {
        Self(
            self.0
                .subwindow(offset, size)
                .expect("GIC sub-frame outside DTB range"),
        )
    }
}

impl GicV3 {
    fn new(info: GicV3Info, possible_cpus: CpuSet) -> Result<Self, InterruptError> {
        if possible_cpus.is_empty() {
            return Err(InterruptError::InvalidVector);
        }
        let distributor = MmioRange::new(
            crate::arch::mmu::physical_to_virtual(info.distributor.start),
            info.distributor.size,
        )?;
        let redistributor = MmioRange::new(
            crate::arch::mmu::physical_to_virtual(info.redistributor.start),
            info.redistributor.size,
        )?;
        let typer = distributor.read32(GICD_TYPER);
        let max_interrupt = (((typer & 0x1f) + 1) * 32 - 1).min(1019);
        if max_interrupt < 32 {
            return Err(InterruptError::InvalidVector);
        }
        let requires_range_selector = possible_cpus
            .iter()
            .any(|cpu| cpu::hardware_id(cpu).raw() & 0xff >= u16::BITS as usize);
        if requires_range_selector && typer & GICD_TYPER_RSS == 0 {
            return Err(InterruptError::InvalidVector);
        }
        Ok(Self {
            distributor,
            redistributor,
            redistributor_physical: info.redistributor.start,
            max_interrupt,
            requires_range_selector,
            possible_cpus,
            handlers: FallibleMap::new(),
        })
    }

    fn initialize_global(&self) {
        self.distributor.write32(GICD_CTLR, 0);
        self.wait_distributor_write();

        let register_count = (self.max_interrupt / 32) as usize + 1;
        for register in 1..register_count {
            let offset = register * core::mem::size_of::<u32>();
            self.distributor.write32(GICD_ICENABLER + offset, u32::MAX);
            self.distributor.write32(GICD_ICACTIVER + offset, u32::MAX);
            self.distributor.write32(GICD_IGROUPR + offset, u32::MAX);
        }
        for register in 8..=(self.max_interrupt as usize / 4) {
            self.distributor
                .write32(GICD_IPRIORITYR + register * 4, 0xa0a0_a0a0);
        }

        self.distributor.write32(GICD_CTLR, GICD_CTLR_ARE_NS);
        self.wait_distributor_write();
        let boot_route = route_value(cpu::hardware_id(cpu::boot_id()).raw());
        for interrupt in 32..=self.max_interrupt {
            self.distributor
                .write64(GICD_IROUTER + interrupt as usize * 8, boot_route);
        }
        self.distributor
            .write32(GICD_CTLR, GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_G1_NS);
        self.wait_distributor_write();
    }

    fn initialize_local(&self) {
        let hardware_id = cpu::executing_hardware_id().raw();
        let redistributor = self.redistributor_for(hardware_id);
        let mut waker = redistributor.read32(GICR_WAKER);
        waker &= !GICR_WAKER_PROCESSOR_SLEEP;
        redistributor.write32(GICR_WAKER, waker);
        while redistributor.read32(GICR_WAKER) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
            core::hint::spin_loop();
        }

        let sgi = redistributor.subrange(GICR_SGI_BASE, REDISTRIBUTOR_STRIDE - GICR_SGI_BASE);
        sgi.write32(GICD_ICENABLER, u32::MAX);
        sgi.write32(GICD_ICACTIVER, u32::MAX);
        sgi.write32(GICD_IGROUPR, u32::MAX);
        for register in 0..(u32::BITS as usize / 4) {
            sgi.write32(GICD_IPRIORITYR + register * 4, 0x8080_8080);
        }
        sgi.write32(GICD_ISENABLER, (1u32 << TIMER_PPI) | (1u32 << SOFTWARE_SGI));
        while redistributor.read32(GICR_CTLR) & GICR_CTLR_RWP != 0 {
            core::hint::spin_loop();
        }

        // SAFETY: each CPU owns its ICC_*_EL1 interface during local initialization. SRE must be
        // visible before reading ICC_CTLR_EL1 or performing later ICC accesses.
        unsafe {
            asm!(
                "msr icc_sre_el1, {sre}",
                "isb",
                sre = in(reg) 1u64,
                options(nostack)
            );
        }
        let interface_control: u64;
        // SAFETY: SRE is enabled above; ICC_CTLR_EL1 is the calling CPU's read-only interface fact.
        unsafe {
            asm!("mrs {value}, icc_ctlr_el1", value = out(reg) interface_control, options(nomem, nostack, preserves_flags));
        }
        assert!(
            !self.requires_range_selector || interface_control & ICC_CTLR_RSS != 0,
            "CPU interface lacks SGI range selector required by MPIDR topology"
        );
        // SAFETY: PMR admits configured priority 0x80 Group-1 IRQs; the calling CPU uniquely owns
        // its binary-point and Group-1 enable registers during local initialization.
        unsafe {
            asm!(
                "msr icc_pmr_el1, {pmr}",
                "msr icc_bpr1_el1, xzr",
                "msr icc_igrpen1_el1, {enable}",
                "isb",
                pmr = in(reg) 0xffu64,
                enable = in(reg) 1u64,
                options(nostack)
            );
        }
    }

    fn redistributor_for(&self, hardware_id: usize) -> MmioRange {
        self.redistributor_frame(hardware_id).0
    }

    fn redistributor_frame(&self, hardware_id: usize) -> (MmioRange, usize) {
        let expected = packed_affinity(hardware_id);
        let mut offset = 0usize;
        loop {
            let frame = self.redistributor.subrange(offset, REDISTRIBUTOR_STRIDE);
            let typer = frame.read64(GICR_TYPER);
            if (typer >> 32) as u32 == expected {
                return (frame, offset);
            }
            assert!(
                typer & GICR_TYPER_LAST == 0,
                "GICR has no frame for MPIDR {hardware_id:#x}"
            );
            offset = offset
                .checked_add(REDISTRIBUTOR_STRIDE)
                .expect("GICR frame offset overflow");
        }
    }

    fn wait_distributor_write(&self) {
        while self.distributor.read32(GICD_CTLR) & GICD_CTLR_RWP != 0 {
            core::hint::spin_loop();
        }
    }

    fn valid_device_vector(&self, vector: u32) -> bool {
        (32..=self.max_interrupt).contains(&vector)
    }

    fn unregister_handler(&mut self, vector: InterruptVector) -> Option<Arc<dyn InterruptHandler>> {
        if !self.valid_device_vector(vector) && !super::its::contains(vector) {
            return None;
        }
        self.disable_interrupt(vector);
        super::its::clear(vector);
        self.handlers.remove(&vector)
    }

    fn disable_interrupt(&self, vector: InterruptVector) {
        if super::its::set_enabled(vector, false) {
            return;
        }
        let register = vector as usize / 32;
        self.distributor
            .write32(GICD_ICENABLER + register * 4, 1u32 << (vector % 32));
        self.wait_distributor_write();
    }

    fn enable_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        if !self.valid_device_vector(vector) && !super::its::contains(vector) {
            return Err(InterruptError::InvalidVector);
        }
        if !self.handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }
        if super::its::set_enabled(vector, true) {
            return Ok(());
        }
        let register = vector as usize / 32;
        self.distributor
            .write32(GICD_ISENABLER + register * 4, 1u32 << (vector % 32));
        Ok(())
    }

    fn set_priority(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        if !self.valid_device_vector(vector) && !super::its::contains(vector) {
            return Err(InterruptError::InvalidVector);
        }
        if super::its::contains(vector) {
            return Ok(());
        }
        let register = vector as usize / 4;
        let shift = (vector % 4) * 8;
        let offset = GICD_IPRIORITYR + register * 4;
        let current = self.distributor.read32(offset);
        self.distributor
            .write32(offset, (current & !(0xff << shift)) | (0x80 << shift));
        Ok(())
    }

    fn set_affinity(&self, vector: InterruptVector, cpus: CpuSet) -> Result<(), InterruptError> {
        if !self.valid_device_vector(vector)
            || cpus.is_empty()
            || cpus.iter().any(|cpu| !self.possible_cpus.contains(cpu))
        {
            return Err(InterruptError::InvalidVector);
        }
        let mut targets = cpus.iter();
        let target = targets.next().ok_or(InterruptError::InvalidVector)?;
        if targets.next().is_some() {
            return Err(InterruptError::InvalidVector);
        }
        self.distributor.write64(
            GICD_IROUTER + vector as usize * 8,
            route_value(cpu::hardware_id(target).raw()),
        );
        Ok(())
    }

    fn claim(&self) -> ClaimedInterrupt {
        let acknowledge: u64;
        // SAFETY: local ICC interface was initialized before IRQ delivery; IAR read creates the
        // active interrupt token which must be returned exactly once to complete().
        unsafe {
            asm!("mrs {value}, icc_iar1_el1", value = out(reg) acknowledge, options(nomem, nostack, preserves_flags));
        }
        let interrupt = acknowledge as u32 & 0x00ff_ffff;
        if (SPURIOUS_INTERRUPT_MIN..=1023).contains(&interrupt) {
            return ClaimedInterrupt::Spurious;
        }
        match interrupt {
            TIMER_PPI => ClaimedInterrupt::Timer(interrupt),
            SOFTWARE_SGI => ClaimedInterrupt::Software(interrupt),
            device => {
                if let Some(handler) = self.handlers.get(&device).cloned() {
                    if handler.masks_until_ack() {
                        self.disable_interrupt(device);
                    }
                    if let Err(_error) = handler.handle_interrupt(device) {
                        #[cfg(debug_assertions)]
                        crate::debug!("GIC handler {} failed: {:?}", device, _error);
                    }
                } else {
                    crate::error!("unregistered GIC interrupt {}", device);
                }
                ClaimedInterrupt::Device(device)
            }
        }
    }

    fn complete(&self, interrupt: u32) {
        // SAFETY: interrupt came from this CPU's ICC_IAR1_EL1 and is consumed exactly once by the
        // linear ClaimedInterrupt handoff. EOImode=0 performs priority drop and deactivation.
        unsafe {
            asm!("msr icc_eoir1_el1, {value}", value = in(reg) interrupt as u64, options(nomem, nostack, preserves_flags));
        }
    }
}

pub(crate) fn initialize(info: GicV3Info) -> Result<(), InterruptError> {
    let controller = GicV3::new(info, cpu::possible())?;
    controller.initialize_global();
    GIC.call_once(|| IrqMutex::new(controller));
    initialize_local();
    Ok(())
}

pub(crate) fn initialize_local() {
    GIC.wait().lock().initialize_local();
}

pub(crate) fn register_device(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    affinity: CpuSet,
) -> Result<(), InterruptError> {
    let prepared =
        FallibleMap::try_prepare(vector, handler).map_err(|_| InterruptError::NoMemory)?;
    let mut controller = GIC.wait().lock();
    if !controller.valid_device_vector(vector) || controller.handlers.contains_key(&vector) {
        return Err(InterruptError::InvalidVector);
    }
    controller.handlers.commit_vacant(prepared);
    controller.set_priority(vector)?;
    controller.set_affinity(vector, affinity)?;
    controller.enable_interrupt(vector)
}

/// 绑定设备中断到 boot CPU；失败时不留下部分状态。
///
/// # Errors
/// 向量非法、已被占用或分配失败。
pub(crate) fn bind_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    level: bool,
    _acknowledge_local: impl Fn(),
) -> Result<(), native_abi::Status> {
    if !GIC.wait().lock().valid_device_vector(vector) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    bind_source(vector, handler, Some(level))
}

/// 仅绑定已分配的 LPI；其默认 collection 已由 ITS 安装。
pub(crate) fn bind_message_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    _acknowledge_local: impl Fn(),
) -> Result<(), native_abi::Status> {
    if !super::its::contains(vector) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    bind_source(vector, handler, None)
}

fn bind_source(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    level: Option<bool>,
) -> Result<(), native_abi::Status> {
    let prepared =
        FallibleMap::try_prepare(vector, handler).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut controller = GIC.wait().lock();
    if controller.handlers.contains_key(&vector) {
        return Err(native_abi::ERR_ALREADY_BOUND);
    }
    controller.handlers.commit_vacant(prepared);
    let result = controller
        .set_priority(vector)
        .and_then(|()| {
            if let Some(level) = level {
                let offset = GICD_ICFGR + vector as usize / 16 * 4;
                let bit = 1 << (vector % 16 * 2 + 1);
                let old = controller.distributor.read32(offset);
                controller
                    .distributor
                    .write32(offset, if level { old & !bit } else { old | bit });
                if (controller.distributor.read32(offset) & bit == 0) != level {
                    return Err(InterruptError::InvalidVector);
                }
                controller.set_affinity(vector, CpuSet::singleton(cpu::boot_id()))
            } else {
                Ok(())
            }
        })
        .and_then(|()| controller.enable_interrupt(vector));
    if result.is_err() {
        let handler = controller.unregister_handler(vector);
        drop(controller);
        drop(handler);
    }
    result.map_err(binding_status)
}

/// 解除绑定；返回的 handler 在锁外释放。
pub(crate) fn unbind_interrupt(
    vector: u32,
    _retirement: &mut InterruptRetirement,
    _acknowledge_local: impl Fn(),
) -> Option<Arc<dyn InterruptHandler>> {
    GIC.wait().lock().unregister_handler(vector)
}

/// 解除 `masks_until_ack` 留下的屏蔽。
pub(crate) fn unmask_interrupt(vector: u32) -> Result<(), InterruptError> {
    GIC.wait().lock().enable_interrupt(vector)
}

/// 改变 SPI 的单目标路由，保留 masks_until_ack 产生的屏蔽状态。
pub(crate) fn set_interrupt_affinity(
    vector: u32,
    cpu: cpu::CpuId,
    _acknowledge_local: impl Fn(),
) -> Result<(), native_abi::Status> {
    let controller = GIC.wait().lock();
    if !controller.valid_device_vector(vector) || !controller.possible_cpus.contains(cpu) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    if !controller.handlers.contains_key(&vector) {
        return Err(native_abi::ERR_BAD_STATE);
    }
    // 1. 与固定 Linux gic_set_affinity 一致，先停止已启用源，再更改 IROUTER。
    let enabled = controller
        .distributor
        .read32(GICD_ISENABLER + vector as usize / 32 * 4)
        & (1u32 << (vector % 32))
        != 0;
    if enabled {
        controller.disable_interrupt(vector);
    }
    // 2. 路由变化不消费 pending 事件；原本待 ack 的电平源必须继续屏蔽。
    let result = controller.set_affinity(vector, CpuSet::singleton(cpu));
    if enabled {
        controller
            .enable_interrupt(vector)
            .map_err(binding_status)?;
    } else {
        controller.wait_distributor_write();
    }
    result.map_err(binding_status)
}

/// 设备中断（SPI）向量区间。
pub(crate) fn device_interrupt_range() -> core::ops::Range<u32> {
    32..GIC.wait().lock().max_interrupt + 1
}

pub(crate) fn claim_interrupt() -> ClaimedInterrupt {
    GIC.wait().lock().claim()
}

pub(crate) fn complete_interrupt(claim: ClaimedInterrupt) {
    let Some(interrupt) = claim.completion_token() else {
        return;
    };
    GIC.wait().lock().complete(interrupt);
}

pub(crate) fn send_ipi(cpus: CpuSet) -> Result<(), InterruptError> {
    let mut remaining = cpus;
    while let Some(first) = remaining.iter().next() {
        let first_hardware = cpu::hardware_id(first).raw();
        let group = affinity_group(first_hardware);
        let mut target_list = 0u16;
        for target in remaining.iter() {
            let hardware = cpu::hardware_id(target).raw();
            if affinity_group(hardware) == group {
                let aff0 = (hardware & 0xff) as u32;
                target_list |= 1u16 << (aff0 % u16::BITS);
                remaining.remove(target);
            }
        }
        crate::arch::interrupt::send_software_sgi(first_hardware, target_list);
    }
    Ok(())
}

fn packed_affinity(hardware: usize) -> u32 {
    ((hardware & 0x00ff_ffff) | ((hardware >> 8) & 0xff00_0000)) as u32
}

fn affinity_group(hardware: usize) -> (usize, usize) {
    (hardware & !0xff, (hardware & 0xff) / u16::BITS as usize)
}

fn route_value(hardware: usize) -> u64 {
    (hardware as u64) & 0x0000_00ff_00ff_ffff
}

/// 一颗 CPU 的 LPI 接收端；由 GIC 的帧发现投影给 ITS，不复制寄存器配置。
pub(super) struct LpiTarget {
    pub(super) cpu: cpu::CpuId,
    pub(super) bus: MmioBus,
    pub(super) target: u64,
}

/// 投影 LPI 位宽与每颗逻辑 CPU 的 redistributor/collection target；仅冷初始化分配。
pub(super) fn lpi_redistributors(
    physical_target: bool,
) -> Result<(u32, alloc::vec::Vec<LpiTarget>), native_abi::Status> {
    let mut targets = alloc::vec::Vec::new();
    targets
        .try_reserve_exact(cpu::possible().iter().count())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let controller = GIC.wait().lock();
    let typer = controller.distributor.read32(GICD_TYPER);
    let bits = ((typer >> 19) & 31) + 1;
    if typer & (1 << 17) == 0 || bits < 14 {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    for cpu in controller.possible_cpus.iter() {
        let (frame, offset) = controller.redistributor_frame(cpu::hardware_id(cpu).raw());
        if frame.read64(GICR_TYPER) & 1 == 0 {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let target = if physical_target {
            (controller.redistributor_physical + offset) as u64
        } else {
            ((frame.read64(GICR_TYPER) >> 8) & 0xffff) << 16
        };
        targets.push(LpiTarget {
            cpu,
            bus: frame.0,
            target,
        });
    }
    Ok((bits, targets))
}

fn binding_status(error: InterruptError) -> native_abi::Status {
    match error {
        InterruptError::NoMemory => native_abi::ERR_NO_MEMORY,
        InterruptError::InvalidVector => native_abi::ERR_INVALID_ARGS,
        InterruptError::HandlerNotSet => native_abi::ERR_BAD_STATE,
    }
}
