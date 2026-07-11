use alloc::{boxed::Box, vec::Vec};
use core::{
    mem::{MaybeUninit, offset_of, size_of},
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};

use spin::Once;

use crate::memory::KERNEL_STACK_SIZE;

const UNPUBLISHED_TABLE: usize = usize::MAX;
const TIMER_SOFTIRQ: u32 = 1;

// OWNER: hart module owns the immutable DTB-derived topology and per-hart states.
static HART_TOPOLOGY: Once<HartTopology> = Once::new();

// 非零哨兵把变量放入 .data。cold-boot `_start` 在 BSS 清零前读取它；若使用零初始化，
// 未清零 BSS 可能让入口把首个 hart 误判为 secondary 并解引用无效表地址。
// OWNER: boot hart publishes the topology table address consumed by secondary entry assembly.
pub(crate) static HART_TABLE_ADDRESS: AtomicUsize = AtomicUsize::new(UNPUBLISHED_TABLE);
// OWNER: boot hart publishes the matching topology table length with the address.
pub(crate) static HART_TABLE_LENGTH: AtomicUsize = AtomicUsize::new(UNPUBLISHED_TABLE);

/// @description DTB hart 的动态启动栈。
#[repr(C, align(4096))]
struct StartupStack([MaybeUninit<u8>; KERNEL_STACK_SIZE]);

/// @description 一个 DTB hart 的所有 kernel-local 状态。
#[repr(C, align(64))]
pub(crate) struct HartState {
    hart_id: usize,
    startup_stack_top: usize,
    _startup_stack: Box<StartupStack>,
    softirq_pending: AtomicU32,
    online: AtomicBool,
    active: AtomicBool,
}

impl HartState {
    fn new(hart_id: usize) -> Self {
        // SAFETY: `StartupStack` 只包含 `MaybeUninit<u8>`，任意位模式均有效；启动栈会在写入后才读取。
        let startup_stack = unsafe { Box::<StartupStack>::new_uninit().assume_init() };
        let startup_stack_top = startup_stack.0.as_ptr() as usize + KERNEL_STACK_SIZE;
        Self {
            hart_id,
            startup_stack_top,
            _startup_stack: startup_stack,
            softirq_pending: AtomicU32::new(0),
            online: AtomicBool::new(false),
            active: AtomicBool::new(false),
        }
    }

    /// @description 获取该状态所属的 DTB hart ID。
    ///
    /// @return 原始 hart ID。
    /// @errors 无错误。
    pub(crate) fn hart_id(&self) -> usize {
        self.hart_id
    }

    /// @description 发布该 hart 已具备接收 IPI/RFENCE 的条件。
    ///
    /// @return 无返回值。
    /// @errors 无错误。
    pub(crate) fn mark_online(&self) {
        self.online.store(true, Ordering::Release);
    }

    /// @description 查询该 hart 是否已具备接收 IPI/RFENCE 的条件。
    ///
    /// @return `mark_online` 已发布时返回 `true`。
    /// @errors 无错误。
    pub(crate) fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    /// @description 发布该 hart 已进入 scheduler/idle 循环。
    ///
    /// @return 无返回值。
    /// @errors 无错误。
    pub(crate) fn mark_active(&self) {
        self.active.store(true, Ordering::Release);
    }

    /// @description 查询该 hart 是否可接收 scheduler mailbox。
    ///
    /// @return `mark_active` 已发布时返回 `true`。
    /// @errors 无错误。
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

pub(crate) const HART_STATE_SIZE: usize = size_of::<HartState>();
pub(crate) const HART_STATE_ID_OFFSET: usize = offset_of!(HartState, hart_id);
pub(crate) const HART_STATE_STACK_TOP_OFFSET: usize = offset_of!(HartState, startup_stack_top);

/// @description kernel 从 DTB 构造的唯一 hart 拓扑及其 per-hart owner。
pub(crate) struct HartTopology {
    hart_mask: usize,
    hart_count: usize,
    max_hart_id: usize,
    boot_hart: usize,
    states: Box<[HartState]>,
}

/// @description 在 allocator 可用前验证 cold-boot hart 与 DTB CPU 描述。
///
/// @param board_info 已解析的 DTB board 信息。
/// @param boot_hart 首个进入 kernel 的 hart ID。
/// @return 无返回值。
/// @errors 空 CPU 集、超出 SBI mask、重复 hart ID 或 boot hart 缺失时 fail-stop。
pub(crate) fn validate_boot_hart(board_info: &crate::arch::dtb::BoardInfo, boot_hart: usize) {
    assert!(board_info.hart_count != 0, "DTB contains no enabled hart");
    assert!(
        board_info.invalid_hart_id.is_none(),
        "DTB hart ID {} exceeds SBI hart-mask width {}",
        board_info.invalid_hart_id.unwrap_or(usize::MAX),
        usize::BITS
    );
    assert_eq!(
        board_info.hart_mask.count_ones() as usize,
        board_info.hart_count,
        "DTB CPU count and unique hart mask disagree"
    );
    assert!(
        boot_hart < usize::BITS as usize && board_info.hart_mask & (1usize << boot_hart) != 0,
        "boot hart {} is absent from DTB hart mask {:#x}",
        boot_hart,
        board_info.hart_mask
    );
}

/// @description 按 DTB hart mask 分配并发布动态 per-hart 状态。
///
/// @param board_info 已验证的 DTB board 信息。
/// @param boot_hart cold-boot hart ID。
/// @return 无返回值。
/// @errors 重复初始化或分配失败时 fail-stop。
pub(crate) fn init_topology(board_info: &crate::arch::dtb::BoardInfo, boot_hart: usize) {
    assert!(
        HART_TOPOLOGY.get().is_none(),
        "hart topology initialized twice"
    );

    let mut states = Vec::with_capacity(board_info.hart_count);
    let mut mask = board_info.hart_mask;
    while mask != 0 {
        let hart_id = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        states.push(HartState::new(hart_id));
    }
    assert_eq!(states.len(), board_info.hart_count);

    let topology = HART_TOPOLOGY.call_once(|| HartTopology {
        hart_mask: board_info.hart_mask,
        hart_count: board_info.hart_count,
        max_hart_id: board_info.max_hart_id,
        boot_hart,
        states: states.into_boxed_slice(),
    });

    // 1. 表长度先写入；它只由随后 address 的 Release 发布。
    HART_TABLE_LENGTH.store(topology.states.len(), Ordering::Relaxed);
    // 2. secondary `_start` 的 acquire fence 消费表内容与长度，缺失时可能看到未构造的 stack top。
    HART_TABLE_ADDRESS.store(topology.states.as_ptr() as usize, Ordering::Release);
}

/// @description 判断动态 hart table 是否已发布。
///
/// @return allocator 后的拓扑初始化完成时返回 `true`。
/// @errors 无错误。
pub(crate) fn topology_ready() -> bool {
    HART_TABLE_ADDRESS.load(Ordering::Acquire) != UNPUBLISHED_TABLE
}

fn topology() -> &'static HartTopology {
    assert!(topology_ready(), "hart topology used before allocator init");
    HART_TOPOLOGY.wait()
}

/// @description 读取未经验证的当前 hart ID，仅供入口检查和 panic 诊断。
///
/// @return `tp` 中由内核入口安装的原始 hart ID。
#[inline(always)]
pub(crate) fn raw_hart_id() -> usize {
    let value: usize;
    // SAFETY: `_start` installs the current SBI hart ID in `tp` before entering Rust and no
    // kernel code repurposes `tp`; the instruction reads only this hart-local register.
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) value, options(nomem, nostack));
    }
    value
}

/// @description 获取已经过动态拓扑验证的当前 hart ID。
///
/// @return 已存在于 DTB hart table 的 hart ID。
/// @errors `tp` 不在 DTB table 中表示入口或 trap 上下文被破坏，将触发 panic。
#[inline(always)]
pub(crate) fn hart_id() -> usize {
    let hart = raw_hart_id();
    assert!(
        state(hart).is_some(),
        "hart invariant violated: tp={} not in DTB mask {:#x}",
        hart,
        possible_hart_mask()
    );
    hart
}

/// @description 发布当前 hart 的 deferred timer work 并触发 supervisor software interrupt。
///
/// @return 无返回值。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn raise_timer_softirq() {
    state(hart_id())
        .expect("softirq hart disappeared from topology")
        .softirq_pending
        .fetch_or(TIMER_SOFTIRQ, Ordering::Release);
    // SAFETY: kernel runs in S-mode and sets only the current hart's supervisor software pending bit.
    unsafe { riscv::register::sip::set_ssoft() }
}

/// @description 原子消费当前 hart 的 deferred timer work pending bit。
///
/// @return 本次调用是否取得 timer work。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn take_timer_softirq() -> bool {
    // SAFETY: deferred-work consumer clears only the current hart's supervisor software bit;
    // ordinary IPI carries no payload and serves only to wake this same consumer.
    unsafe { riscv::register::sip::clear_ssoft() }
    state(hart_id())
        .expect("softirq hart disappeared from topology")
        .softirq_pending
        .fetch_and(!TIMER_SOFTIRQ, Ordering::AcqRel)
        & TIMER_SOFTIRQ
        != 0
}

/// @description 获取 DTB 描述的 possible hart mask。
///
/// @return bit N 表示 hart N 存在于动态 hart table。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn possible_hart_mask() -> usize {
    topology().hart_mask
}

/// @description 获取 DTB hart 数量。
///
/// @return 动态 hart table 的 entry 数量。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn hart_count() -> usize {
    topology().hart_count
}

/// @description 获取 DTB 最大 hart ID。
///
/// @return DTB hart mask 中最高置位的 ID。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn max_hart_id() -> usize {
    topology().max_hart_id
}

/// @description 获取 cold-boot hart ID。
///
/// @return 首个进入 kernel 的 DTB hart ID。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn boot_hart_id() -> usize {
    topology().boot_hart
}

/// @description 获取所有 DTB hart state，顺序按 hart ID 递增。
///
/// @return 只包含 DTB mask 中 hart 的动态切片。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn states() -> &'static [HartState] {
    &topology().states
}

/// @description 按原始 hart ID 查找动态 state。
///
/// @param hart_id 待查找的 DTB hart ID。
/// @return hart 存在时返回 state，否则返回 `None`。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn state(hart_id: usize) -> Option<&'static HartState> {
    let states = states();
    states
        .binary_search_by_key(&hart_id, HartState::hart_id)
        .ok()
        .map(|index| &states[index])
}

/// @description 发布当前 hart 已完成页表、timer 和中断初始化。
///
/// @return 无返回值。
/// @errors 当前 hart 不属于动态 topology 时 fail-stop。
pub(crate) fn mark_online() {
    state(hart_id())
        .expect("current hart disappeared from topology")
        .mark_online();
}

/// @description 获取已完成 S-mode 初始化的 hart 集合。
///
/// @return bit N 表示 hart N 已可接收 IPI/RFENCE 后续工作。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn online_hart_mask() -> usize {
    states().iter().fold(0, |mask, state| {
        if state.is_online() {
            mask | (1usize << state.hart_id())
        } else {
            mask
        }
    })
}
