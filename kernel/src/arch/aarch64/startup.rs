//! AArch64 secondary-entry topology and CPU-local execution initialization.

use alloc::{boxed::Box, vec::Vec};
use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};
use spin::Once;

use crate::config::KERNEL_STACK_SIZE;

const UNPUBLISHED_TABLE: usize = usize::MAX;

/// Static input binding an MPIDR hardware identity to a logical CPU index.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StartupCpu {
    hardware_id: usize,
    logical_id: usize,
}

impl StartupCpu {
    /// Bind one platform hardware identity to its generic logical identity.
    pub(crate) fn new(hardware_id: usize, logical_id: usize) -> Self {
        Self {
            hardware_id,
            logical_id,
        }
    }
}

#[repr(C, align(4096))]
struct StartupStack([MaybeUninit<u8>; KERNEL_STACK_SIZE]);

#[repr(C, align(64))]
struct StartupEntry {
    hardware_id: usize,
    logical_id: usize,
    stack_top: usize,
    _stack: Box<StartupStack>,
}

impl StartupEntry {
    fn new(cpu: StartupCpu) -> Self {
        // SAFETY: StartupStack contains only MaybeUninit bytes; entry assembly establishes SP
        // before any stack byte is interpreted as a Rust value.
        let stack = unsafe { Box::<StartupStack>::new_uninit().assume_init() };
        let stack_top = stack.0.as_ptr() as usize + KERNEL_STACK_SIZE;
        Self {
            hardware_id: cpu.hardware_id,
            logical_id: cpu.logical_id,
            stack_top,
            _stack: stack,
        }
    }
}

// OWNER: the startup module uniquely retains secondary stacks and the MPIDR/logical projection.
static STARTUP_TOPOLOGY: Once<Box<[StartupEntry]>> = Once::new();
pub(crate) static TABLE_ADDRESS: AtomicUsize = AtomicUsize::new(UNPUBLISHED_TABLE);
pub(crate) static TABLE_LENGTH: AtomicUsize = AtomicUsize::new(UNPUBLISHED_TABLE);
// OWNER: zero is unpublished; otherwise `(IDC | DIC << 1) + 1`. A separate unpublished state is
// required because IDC=false is a valid architectural capability result.
static CACHE_CAPABILITIES: AtomicU8 = AtomicU8::new(0);

pub(crate) const ENTRY_SIZE: usize = core::mem::size_of::<StartupEntry>();
pub(crate) const HARDWARE_ID_OFFSET: usize = core::mem::offset_of!(StartupEntry, hardware_id);
pub(crate) const LOGICAL_ID_OFFSET: usize = core::mem::offset_of!(StartupEntry, logical_id);
pub(crate) const STACK_TOP_OFFSET: usize = core::mem::offset_of!(StartupEntry, stack_top);

const _: () = {
    const WORD: usize = core::mem::size_of::<usize>();
    assert!(HARDWARE_ID_OFFSET == 0);
    assert!(LOGICAL_ID_OFFSET == WORD);
    assert!(STACK_TOP_OFFSET == 2 * WORD);
    assert!(ENTRY_SIZE.is_multiple_of(64));
};

/// Construct and release-publish the immutable secondary startup table.
pub(crate) fn initialize(cpus: impl ExactSizeIterator<Item = StartupCpu>) {
    assert!(
        STARTUP_TOPOLOGY.get().is_none(),
        "startup topology initialized twice"
    );
    assert_ne!(cpus.len(), 0, "startup topology cannot be empty");
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(cpus.len())
        .expect("startup topology allocation failed");
    entries.extend(cpus.map(StartupEntry::new));
    let entries = STARTUP_TOPOLOGY.call_once(|| entries.into_boxed_slice());
    TABLE_LENGTH.store(entries.len(), Ordering::Relaxed);
    TABLE_ADDRESS.store(entries.as_ptr() as usize, Ordering::Release);
}

/// Replace the boot-time MPIDR value in TPIDR_EL1 with the logical CPU identity.
pub(crate) fn install_boot_logical_id(logical_id: usize) {
    // SAFETY: boot CPU exclusively owns TPIDR_EL1 before scheduler publication.
    unsafe {
        core::arch::asm!("msr tpidr_el1, {id}", id = in(reg) logical_id, options(nomem, nostack, preserves_flags))
    };
}

/// Return the calling CPU's logical identity.
#[inline(always)]
pub(crate) fn current_logical_id() -> usize {
    let value: usize;
    // SAFETY: entry/startup installs TPIDR_EL1 and the kernel never repurposes it.
    unsafe {
        core::arch::asm!("mrs {id}, tpidr_el1", id = out(reg) value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// Return the identity currently carried by TPIDR_EL1; before topology publication it is MPIDR.
pub(crate) fn entry_identity() -> usize {
    current_logical_id()
}

pub(crate) fn cache_capabilities() -> (bool, bool) {
    let encoded = CACHE_CAPABILITIES.load(Ordering::Acquire);
    assert_ne!(encoded, 0, "cache capabilities read before initialization");
    let capabilities = encoded - 1;
    (capabilities & 1 != 0, capabilities & 2 != 0)
}

/// Initialize CPU-local AArch64 EL1 execution controls.
pub(crate) fn initialize_local_execution() {
    let uses_sixteen_bit_asids = super::mmu::initialize_address_space_identifiers();

    let ctr: u64;
    // SAFETY: identification register is read-only at EL1.
    unsafe {
        core::arch::asm!("mrs {value}, ctr_el0", value = out(reg) ctr, options(nomem, nostack, preserves_flags))
    };
    let idc = ctr & (1 << 28) != 0;
    let dic = ctr & (1 << 29) != 0;
    let capabilities = 1 + u8::from(idc) + (u8::from(dic) << 1);
    match CACHE_CAPABILITIES.compare_exchange(0, capabilities, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => {}
        Err(published) => assert_eq!(
            published, capabilities,
            "CPUs report inconsistent CTR_EL0.IDC/DIC"
        ),
    }

    let mmfr0: u64;
    // SAFETY: identification register is read-only at EL1.
    unsafe {
        core::arch::asm!("mrs {value}, id_aa64mmfr0_el1", value = out(reg) mmfr0, options(nomem, nostack, preserves_flags))
    };
    // 52-bit-capable CPUs may run a narrower 48-bit output address configuration.
    let parange = (mmfr0 & 0xf).min(5);
    let ips = parange << 32;
    let tcr = 25u64
        | (1 << 8)  // IRGN0 WBWA
        | (1 << 10) // ORGN0 WBWA
        | (3 << 12) // SH0 inner-shareable
        | (25 << 16)
        | (1 << 24) // IRGN1 WBWA
        | (1 << 26) // ORGN1 WBWA
        | (3 << 28) // SH1 inner-shareable
        | (2 << 30) // TG1 4 KiB
        | (u64::from(uses_sixteen_bit_asids) << 36) // AS selects 16-bit TTBR ASIDs
        | ips;
    // SAFETY: CPU is not scheduler-visible. MAIR slot 0 and TCR exactly match the page-table
    // codec. CPACR traps FP/ASIMD in ordinary kernel Rust; bounded assembly windows temporarily
    // enable it only while moving the explicitly owned vector context.
    unsafe {
        core::arch::asm!(
            ".arch_extension pan",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "mrs x9, cpacr_el1",
            "bic x9, x9, #(3 << 20)",
            "msr cpacr_el1, x9",
            "msr pan, #1",
            "isb",
            // AttrIdx0=Normal WBWA (0xff), AttrIdx1=Device-nGnRnE (0x00).
            mair = in(reg) 0x00ffu64,
            tcr = in(reg) tcr,
            out("x9") _,
            options(nostack)
        )
    };
    super::instruction_cache::initialize_local();
}
