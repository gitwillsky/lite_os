//! @description QEMU `virt` GICv3 distributor、redistributor 与 ICC system-register owner。

use alloc::sync::Arc;
use core::arch::asm;

use spin::Once;

use crate::{
    cpu::{self, CpuSet},
    drivers::{InterruptError, InterruptHandler, InterruptVector},
    fallible_tree::FallibleMap,
    sync::IrqMutex,
};

use super::{super::ClaimedInterrupt, discovery::GicV3Info};

const TIMER_PPI: u32 = 27;
const SOFTWARE_SGI: u32 = 1;
const SPURIOUS_INTERRUPT_MIN: u32 = 1020;
const REDISTRIBUTOR_STRIDE: usize = 0x20_000;

const GICD_CTLR: usize = 0x0000;
const GICD_TYPER: usize = 0x0004;
const GICD_IGROUPR: usize = 0x0080;
const GICD_ISENABLER: usize = 0x0100;
const GICD_ICENABLER: usize = 0x0180;
const GICD_ICACTIVER: usize = 0x0380;
const GICD_IPRIORITYR: usize = 0x0400;
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

pub(crate) struct GicV3 {
    distributor: MmioRange,
    redistributor: MmioRange,
    max_interrupt: u32,
    requires_range_selector: bool,
    possible_cpus: CpuSet,
    handlers: FallibleMap<InterruptVector, Arc<dyn InterruptHandler>>,
}

#[derive(Clone, Copy)]
struct MmioRange {
    base: usize,
    end: usize,
}

impl MmioRange {
    fn new(base: usize, size: usize) -> Result<Self, InterruptError> {
        let end = base
            .checked_add(size)
            .filter(|_| base != 0 && size >= core::mem::size_of::<u64>())
            .ok_or(InterruptError::InvalidVector)?;
        Ok(Self { base, end })
    }

    fn address(self, offset: usize, width: usize) -> usize {
        let address = self.base.checked_add(offset).expect("GIC MMIO overflow");
        let access_end = address.checked_add(width).expect("GIC MMIO overflow");
        assert!(access_end <= self.end, "GIC MMIO access exceeds DTB range");
        address
    }

    fn read32(self, offset: usize) -> u32 {
        let address = self.address(offset, core::mem::size_of::<u32>());
        // SAFETY: address is inside the DTB-validated, permanently mapped GIC MMIO range.
        unsafe { crate::arch::read_mmio_u32(address) }
    }

    fn write32(self, offset: usize, value: u32) {
        let address = self.address(offset, core::mem::size_of::<u32>());
        // SAFETY: same bounded GIC MMIO ownership as read32.
        unsafe { crate::arch::write_mmio_u32(address, value) };
    }

    fn read64(self, offset: usize) -> u64 {
        let address = self.address(offset, core::mem::size_of::<u64>());
        assert!(address.is_multiple_of(8), "unaligned GIC 64-bit register");
        // SAFETY: address is aligned and inside the validated GIC MMIO range.
        unsafe { crate::arch::read_mmio_u64(address) }
    }

    fn write64(self, offset: usize, value: u64) {
        let address = self.address(offset, core::mem::size_of::<u64>());
        assert!(address.is_multiple_of(8), "unaligned GIC 64-bit register");
        // SAFETY: address is aligned and inside the validated GIC MMIO range.
        unsafe { crate::arch::write_mmio_u64(address, value) };
    }

    fn subrange(self, offset: usize, size: usize) -> Self {
        let base = self.address(offset, size);
        Self {
            base,
            end: base + size,
        }
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
        let expected = packed_affinity(hardware_id);
        let mut offset = 0usize;
        loop {
            let frame = self.redistributor.subrange(offset, REDISTRIBUTOR_STRIDE);
            let typer = frame.read64(GICR_TYPER);
            if (typer >> 32) as u32 == expected {
                return frame;
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

    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError> {
        if !self.valid_device_vector(vector) {
            return Err(InterruptError::InvalidVector);
        }
        self.handlers
            .try_insert(vector, handler)
            .map_err(|_| InterruptError::NoMemory)?;
        Ok(())
    }

    fn enable_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        if !self.valid_device_vector(vector) {
            return Err(InterruptError::InvalidVector);
        }
        if !self.handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }
        let register = vector as usize / 32;
        self.distributor
            .write32(GICD_ISENABLER + register * 4, 1u32 << (vector % 32));
        Ok(())
    }

    fn set_priority(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        if !self.valid_device_vector(vector) {
            return Err(InterruptError::InvalidVector);
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
        if interrupt >= SPURIOUS_INTERRUPT_MIN {
            return ClaimedInterrupt::from_controller(3, interrupt);
        }
        match interrupt {
            TIMER_PPI => ClaimedInterrupt::from_controller(0, interrupt),
            SOFTWARE_SGI => ClaimedInterrupt::from_controller(2, interrupt),
            device => {
                if let Some(handler) = self.handlers.get(&device).cloned() {
                    if let Err(_error) = handler.handle_interrupt(device) {
                        #[cfg(debug_assertions)]
                        crate::debug!("[Platform] GIC handler {} failed: {:?}", device, _error);
                    }
                } else {
                    crate::error!("[Platform] unregistered GIC interrupt {}", device);
                }
                ClaimedInterrupt::from_controller(1, device)
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
    let mut controller = GIC.wait().lock();
    controller.register_handler(vector, handler)?;
    controller.set_priority(vector)?;
    controller.set_affinity(vector, affinity)?;
    controller.enable_interrupt(vector)
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
        write_sgi(first_hardware, target_list);
    }
    Ok(())
}

pub(crate) fn notify_self() {
    let hardware = cpu::executing_hardware_id().raw();
    let aff0 = (hardware & 0xff) as u32;
    write_sgi(hardware, 1u16 << (aff0 % u16::BITS));
}

fn write_sgi(hardware: usize, targets: u16) {
    let aff1 = (hardware >> 8) & 0xff;
    let aff2 = (hardware >> 16) & 0xff;
    let aff3 = (hardware >> 32) & 0xff;
    let range_selector = ((hardware & 0xff) / u16::BITS as usize) & 0xf;
    let value = (aff3 << 48)
        | (range_selector << 44)
        | (aff2 << 32)
        | ((SOFTWARE_SGI as usize) << 24)
        | (aff1 << 16)
        | targets as usize;
    // SAFETY: target affinity comes from validated CPU topology. DSB publishes prior normal-memory
    // writes before the SGI edge; ICC_SGI1R_EL1 targets Group-1 SGI owned by this controller.
    unsafe {
        asm!(
            "dsb ishst",
            "msr icc_sgi1r_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nostack, preserves_flags)
        );
    }
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
