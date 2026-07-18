//! @description RISC-V secondary CPU entry 所需的 immutable startup topology。

use alloc::{boxed::Box, vec::Vec};
use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};
use spin::Once;

use crate::config::KERNEL_STACK_SIZE;

const UNPUBLISHED_TABLE: usize = usize::MAX;

/// @description 一个 architecture startup slot 的构造输入。
#[derive(Debug, Clone, Copy)]
pub(crate) struct StartupCpu {
    hardware_id: usize,
    logical_id: usize,
}

impl StartupCpu {
    /// @description 绑定 platform hardware identity 与 generic logical CPU identity。
    pub(crate) fn new(hardware_id: usize, logical_id: usize) -> Self {
        Self {
            hardware_id,
            logical_id,
        }
    }
}

#[repr(C, align(4096))]
struct StartupStack([MaybeUninit<u8>; KERNEL_STACK_SIZE]);

/// @description secondary naked entry 唯一消费的 CPU identity 与独占栈。
#[repr(C, align(64))]
struct StartupEntry {
    hardware_id: usize,
    logical_id: usize,
    stack_top: usize,
    _stack: Box<StartupStack>,
}

impl StartupEntry {
    fn new(cpu: StartupCpu) -> Self {
        // SAFETY: `StartupStack` only contains MaybeUninit bytes; entry assembly establishes the
        // stack pointer before any byte is read as a Rust value.
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

// OWNER: startup module uniquely retains entry stacks and the hardware/logical projection.
static STARTUP_TOPOLOGY: Once<Box<[StartupEntry]>> = Once::new();

// 非零哨兵把变量放入 .data；cold boot 在 BSS 清零前读取，零值会被误认成有效表。
// OWNER: startup module publishes the address/length pair consumed by naked secondary entry.
pub(crate) static TABLE_ADDRESS: AtomicUsize = AtomicUsize::new(UNPUBLISHED_TABLE);
pub(crate) static TABLE_LENGTH: AtomicUsize = AtomicUsize::new(UNPUBLISHED_TABLE);

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

/// @description 一次性构造并发布 secondary startup table。
///
/// @param cpus 按 logical ID 排序且 hardware ID 唯一的 startup inputs。
/// @return 无返回值。
/// @errors 空 topology、重复初始化或 allocation failure 时 fail-stop。
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

    // 1. length is part of the immutable table publication and is written before the address.
    TABLE_LENGTH.store(entries.len(), Ordering::Relaxed);
    // 2. secondary acquire fence consumes entries and length after observing the address.
    TABLE_ADDRESS.store(entries.as_ptr() as usize, Ordering::Release);
}

/// @description 将 boot CPU 的 tp 从 firmware hardware ID 切换为 generic logical ID。
pub(crate) fn install_boot_logical_id(logical_id: usize) {
    // SAFETY: boot CPU owns tp before entering scheduler; all later kernel code interprets tp as
    // logical CpuId, matching the value installed by secondary entry assembly.
    unsafe { core::arch::asm!("mv tp, {}", in(reg) logical_id, options(nomem, nostack)) };
}

/// @description 读取 calling CPU 的 generic logical ID。
#[inline(always)]
pub(crate) fn current_logical_id() -> usize {
    let logical_id: usize;
    // SAFETY: startup installs tp once and kernel code never repurposes it.
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) logical_id, options(nomem, nostack));
    }
    logical_id
}

/// @description 读取 entry 当前保存在 tp 的 identity；startup publication 前为 hardware ID。
pub(crate) fn entry_identity() -> usize {
    current_logical_id()
}

/// @description 初始化当前 CPU 的 RISC-V execution status。
///
/// @return 无返回值。
/// @errors 仅允许在 S-mode CPU-local 初始化路径调用。
pub(crate) fn initialize_local_execution() {
    // SAFETY: kernel runs in S-mode and updates only the current CPU's floating-point status.
    unsafe { riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Dirty) };
}
