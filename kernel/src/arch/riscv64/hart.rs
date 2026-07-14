use alloc::{boxed::Box, vec::Vec};
use core::{
    mem::{MaybeUninit, offset_of, size_of},
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use spin::Once;

use crate::memory::KERNEL_STACK_SIZE;

mod memory_barrier;
pub(crate) use memory_barrier::{complete_pending_memory_barrier, synchronize_memory_barrier};

const UNPUBLISHED_TABLE: usize = usize::MAX;
const HART_ID_CAPACITY: usize = usize::BITS as usize;
const NO_HART_INDEX: u8 = u8::MAX;
/// Timer deadline 到期后的 deferred work bit。
pub(crate) const TIMER_SOFTIRQ: u32 = 1;
/// UART RX hardirq 发布的 deferred console wake bit。
pub(crate) const CONSOLE_SOFTIRQ: u32 = 1 << 1;
/// VirtIO-net RX hardirq 发布的 deferred protocol work bit。
pub(crate) const NETWORK_SOFTIRQ: u32 = 1 << 2;
/// Timer/deadline batch 用尽后发布的有界续批 work bit。
pub(crate) const TIMER_BACKLOG_SOFTIRQ: u32 = 1 << 3;
/// VirtIO-GPU controlq hardirq 发布的 deferred completion bit。
pub(crate) const DISPLAY_SOFTIRQ: u32 = 1 << 4;
/// VirtIO-input eventq hardirq 发布的 deferred evdev work bit。
pub(crate) const INPUT_SOFTIRQ: u32 = 1 << 5;

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
    // OWNER: 仅所属 hart 推进自己的 timer deadline；Atomic 为共享 HartState 提供 interior mutability。
    // 若每次从中断处理完成时刻重新起算，handler 延迟会持续累积并让调度 tick 漂移。
    timer_deadline: AtomicU64,
    // OWNER: HartTopology 的每个 slot 保存该 hart 应完成/已完成的同步屏障 generation。
    // 若请求或完成值另存于 syscall/task，会在 IPI 合并或并发调用时丢失确认并永久等待。
    memory_barrier_request: AtomicU64,
    memory_barrier_complete: AtomicU64,
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
            timer_deadline: AtomicU64::new(0),
            memory_barrier_request: AtomicU64::new(0),
            memory_barrier_complete: AtomicU64::new(0),
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
    fn mark_online(&self) {
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
    fn mark_active(&self) {
        self.active.store(true, Ordering::Release);
    }

    /// @description 查询该 hart 是否可接收 scheduler mailbox。
    ///
    /// @return `mark_active` 已发布时返回 `true`。
    /// @errors 无错误。
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// @description 沿固定时钟相位推进该 hart 的下一次 timer deadline。
    ///
    /// @param now 当前 DTB time counter。
    /// @param interval 非零 timer tick 间隔。
    /// @return 严格晚于 `now` 的下一 deadline；尚未初始化时从 `now` 起算首 tick。
    /// @errors interval 为零或 time counter 可表达范围耗尽时 fail-stop。
    fn advance_timer_deadline(&self, now: u64, interval: u64) -> u64 {
        assert_ne!(interval, 0, "timer interval must be non-zero");
        // 1. 零值只表示该 hart 尚未 arm 首次 tick；之后始终以上一次 deadline 为相位基准。
        let previous = self.timer_deadline.load(Ordering::Relaxed);
        // 2. 若 handler 跨过多个周期，一步跳到 now 之后，避免补发 interrupt storm。
        let next = match previous {
            0 => now.checked_add(interval),
            deadline if deadline > now => Some(deadline),
            deadline => (now - deadline)
                .checked_div(interval)
                .and_then(|elapsed| elapsed.checked_add(1))
                .and_then(|periods| periods.checked_mul(interval))
                .and_then(|advance| deadline.checked_add(advance)),
        }
        .expect("timer deadline exhausted the time counter");
        // 3. 仅所属 hart 读写该 slot，Relaxed 足以维持本地 deadline 序列。
        self.timer_deadline.store(next, Ordering::Relaxed);
        next
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
    // OWNER: HartTopology 与有序 states 一次性发布 raw-ID → compact-index immutable projection。
    // 缺失 projection 会让每次 trap/scheduler hart-local 访问重复二分；若独立可变会把 raw ID
    // 路由到错误 state。SBI 单字 mask 将 raw ID 限制在此固定 64-entry RV64 table 内。
    index_by_hart: [u8; HART_ID_CAPACITY],
    states: Box<[HartState]>,
}

impl HartTopology {
    #[inline(always)]
    fn index_of(&self, hart_id: usize) -> Option<usize> {
        let index = *self.index_by_hart.get(hart_id)?;
        (index != NO_HART_INDEX).then_some(index as usize)
    }
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

    let mut states = Vec::new();
    states
        .try_reserve_exact(board_info.hart_count)
        .expect("hart topology allocation failed");
    let mut index_by_hart = [NO_HART_INDEX; HART_ID_CAPACITY];
    let mut mask = board_info.hart_mask;
    while mask != 0 {
        let hart_id = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        index_by_hart[hart_id] =
            u8::try_from(states.len()).expect("hart compact index exceeds projection width");
        states.push(HartState::new(hart_id));
    }
    assert_eq!(states.len(), board_info.hart_count);

    let topology = HART_TOPOLOGY.call_once(|| HartTopology {
        hart_mask: board_info.hart_mask,
        hart_count: board_info.hart_count,
        max_hart_id: board_info.max_hart_id,
        boot_hart,
        index_by_hart,
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

#[inline(always)]
fn current_entry() -> (usize, &'static HartState) {
    let hart = raw_hart_id();
    let topology = topology();
    let index = topology.index_of(hart).unwrap_or_else(|| {
        panic!(
            "hart invariant violated: tp={} not in DTB mask {:#x}",
            hart, topology.hart_mask
        )
    });
    (index, &topology.states[index])
}

#[inline(always)]
fn current_state() -> &'static HartState {
    current_entry().1
}

/// @description 获取已经过动态拓扑验证的当前 hart ID。
///
/// @return 已存在于 DTB hart table 的 hart ID。
/// @errors `tp` 不在 DTB table 中表示入口或 trap 上下文被破坏，将触发 panic。
#[inline(always)]
pub(crate) fn hart_id() -> usize {
    current_state().hart_id()
}

/// @description 获取已经过动态拓扑验证的 calling hart 紧凑 index。
///
/// @return 按原始 hart ID 升序排列的零基 index。
/// @errors `tp` 不在 DTB table 中表示入口或 trap 上下文被破坏，将触发 panic。
#[inline(always)]
pub(crate) fn current_hart_index() -> usize {
    current_entry().0
}

/// @description 沿 calling hart 的固定相位推进 timer deadline。
///
/// @param now 当前 DTB time counter。
/// @param interval 非零 timer tick 间隔。
/// @return 严格晚于 `now` 的下一 deadline；尚未初始化时从 `now` 起算首 tick。
/// @errors `tp` 越界、interval 为零或 time counter 耗尽时 fail-stop。
pub(crate) fn advance_timer_deadline(now: u64, interval: u64) -> u64 {
    current_state().advance_timer_deadline(now, interval)
}

/// @description 发布当前 hart 的 deferred work bitset 并触发 supervisor software interrupt。
///
/// @param work 不为空且只包含已定义的 work bits。
/// @return 无返回值。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
fn raise_softirq(work: u32) {
    assert_ne!(work, 0, "cannot raise an empty softirq");
    current_state()
        .softirq_pending
        .fetch_or(work, Ordering::Release);
    // SAFETY: kernel runs in S-mode and sets only the current hart's supervisor software pending bit.
    unsafe { riscv::register::sip::set_ssoft() }
}

/// @description 发布当前 hart 的 deferred timer work。
///
/// @return 无返回值。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn raise_timer_softirq() {
    raise_softirq(TIMER_SOFTIRQ);
}

/// @description 发布当前 hart 的 deferred console wake work 并触发 SSIP。
///
/// @return 无返回值。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn raise_console_softirq() {
    raise_softirq(CONSOLE_SOFTIRQ);
}

/// @description 发布当前 hart 的 deferred network RX work。
///
/// @return 无返回值。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn raise_network_softirq() {
    raise_softirq(NETWORK_SOFTIRQ);
}

/// @description 发布当前 hart 的 deferred display completion work。
/// @return 无返回值；重复 IRQ 合并为同一个 per-hart bit。
pub(crate) fn raise_display_softirq() {
    raise_softirq(DISPLAY_SOFTIRQ);
}

/// @description 发布当前 hart 的 input event deferred work。
/// @return 无返回值；重复 IRQ 合并为同一个 per-hart bit。
pub(crate) fn raise_input_softirq() {
    raise_softirq(INPUT_SOFTIRQ);
}

/// @description 发布当前 hart 的 timer/deadline backlog 续批工作。
///
/// @return 无返回值；pending bit 合并重复请求，避免无界 nested drain。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn raise_timer_backlog_softirq() {
    raise_softirq(TIMER_BACKLOG_SOFTIRQ);
}

/// @description 原子消费当前 hart 的全部 deferred work pending bits。
///
/// @return 本次调用取得的 work bitset。
/// @errors 当前 hart 不在 DTB topology 时 fail-stop。
pub(crate) fn take_softirqs() -> u32 {
    // SAFETY: deferred-work consumer clears only the current hart's supervisor software bit;
    // ordinary IPI carries no payload and serves only to wake this same consumer.
    unsafe { riscv::register::sip::clear_ssoft() }
    current_state().softirq_pending.swap(0, Ordering::AcqRel)
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

/// @description 将原始 DTB hart ID 映射为有序 topology 中的紧凑 index。
///
/// @param hart_id 待查找的 DTB hart ID。
/// @return hart 存在时返回按 hart ID 升序排列的零基 index，否则返回 `None`。
/// @errors topology 尚未发布时 fail-stop。
pub(crate) fn hart_index(hart_id: usize) -> Option<usize> {
    topology().index_of(hart_id)
}

/// @description 发布当前 hart 已完成页表、timer 和中断初始化。
///
/// @return 无返回值。
/// @errors 当前 hart 不属于动态 topology 时 fail-stop。
pub(crate) fn mark_online() {
    current_state().mark_online();
}

/// @description 发布 calling hart 已进入 scheduler/idle 循环。
///
/// @return 无返回值。
/// @errors 当前 hart 不属于动态 topology 时 fail-stop。
pub(crate) fn mark_active() {
    current_state().mark_active();
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
