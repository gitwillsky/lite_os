//! @description RISC-V supervisor local interrupt mechanism。

/// @description 调用前的 local interrupt enable 状态。
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalInterruptState {
    enabled: bool,
}

/// @description 关闭 calling CPU 的 supervisor interrupt 并保存原状态。
///
/// @return 只能用于同一 CPU 恢复的 opaque 状态。
/// @errors 必须在 S-mode kernel context 调用。
#[inline(always)]
pub(crate) fn disable_local() -> LocalInterruptState {
    let enabled = riscv::register::sstatus::read().sie();
    // SAFETY: kernel runs in S-mode and updates only the calling CPU's SIE bit.
    unsafe { riscv::register::sstatus::clear_sie() };
    LocalInterruptState { enabled }
}

/// @description 恢复 calling CPU 的 local interrupt enable 状态。
///
/// @param state 同一 CPU 上由 `disable_local` 取得的 opaque 状态。
/// @return 无返回值。
/// @errors 跨 CPU 使用会破坏 local interrupt invariant；RAII guard 从类型层禁止移动。
#[inline(always)]
// SAFETY: caller must return the opaque state to the same CPU that produced it.
pub(crate) unsafe fn restore_local(state: LocalInterruptState) {
    if state.enabled {
        // SAFETY: caller proves that `state` belongs to this CPU and only local SIE is updated.
        unsafe { riscv::register::sstatus::set_sie() };
    }
}

/// @description 启用 scheduler 运行所需的 supervisor software/external/global interrupts。
///
/// @return 无返回值。
/// @errors local trap vector 与 interrupt controller 必须已初始化。
// SAFETY: caller must establish the documented local initialization order.
pub(crate) unsafe fn enable_scheduler_interrupts() {
    // SAFETY: caller establishes the initialization ordering documented above.
    unsafe {
        riscv::register::sie::set_ssoft();
        riscv::register::sie::set_sext();
        riscv::register::sstatus::set_sie();
    }
}

/// @description 以不丢唤醒的顺序等待一次启动期外部中断。
///
/// architecture assembly 临时打开 SSIE/SIE 并执行带固定 resume label 的 WFI；
/// hardirq 发布的 pending SSIP 是已确认 device edge 的耐久 wake token。若 external
/// 或 software trap 命中 enable-to-WFI 窗口，kernel trap entry 把 `sepc` 精确推进到
/// resume label，禁止确认唯一 IRQ edge 后重新睡眠。返回前精确恢复调用时
/// 的 SEIE/SSIE/SIE；timer source 始终不变。
///
/// @return 一次 WFI 返回后无返回值。
/// @errors local trap vector、interrupt controller 与 membarrier per-CPU state 必须已初始化；
/// SSIE/timer source 必须在 scheduler owner 尚未初始化时保持关闭。
pub(crate) fn wait_for_external_interrupt() {
    let local = disable_local();
    assert!(
        !riscv::register::sie::read().ssoft(),
        "bootstrap external wait requires scheduler software IRQ source disabled"
    );
    let external_enabled = riscv::register::sie::read().sext();
    if !external_enabled {
        // SAFETY: kernel runs in S-mode and changes only the calling CPU's SEIE source.
        unsafe { riscv::register::sie::set_sext() };
    }
    // SAFETY: trap.S temporarily owns local SIE and its trap-entry resume fixup; caller established
    // the trap/PLIC initialization and bootstrap source constraints documented above.
    unsafe extern "C" {
        fn __wait_for_external_interrupt();
    }
    // SAFETY: local SIE/SSIE are closed and SEIE is enabled; assembly restores SIE/SSIE before return.
    unsafe { __wait_for_external_interrupt() };
    if !external_enabled {
        // SAFETY: kernel restores only the calling CPU's previously disabled SEIE source.
        unsafe { riscv::register::sie::clear_sext() };
    }
    // SAFETY: local was captured on this CPU and no context switch occurs in bootstrap wait.
    unsafe { restore_local(local) };
}

/// @description 启用 calling CPU 的 supervisor timer interrupt source。
///
/// @return 无返回值。
/// @errors 首个 timer deadline 必须在 global interrupt enable 前完成 programming。
// SAFETY: caller must program the first deadline before enabling global delivery.
pub(crate) unsafe fn enable_timer_source() {
    // SAFETY: caller owns local timer initialization ordering.
    unsafe { riscv::register::sie::set_stimer() };
}

/// @description 发布 calling CPU 的 software interrupt pending bit。
#[inline(always)]
pub(crate) fn raise_software() {
    // SAFETY: kernel sets only the calling CPU's supervisor software pending bit.
    unsafe { riscv::register::sip::set_ssoft() }
}

/// @description 清除 calling CPU 的 software interrupt pending bit。
#[inline(always)]
pub(crate) fn clear_software() {
    // SAFETY: kernel clears only the calling CPU's supervisor software pending bit.
    unsafe { riscv::register::sip::clear_ssoft() }
}

/// @description 关闭 local interrupts，用于 panic fail-stop 路径。
pub(crate) fn disable_for_fail_stop() {
    // SAFETY: panic handling never restores local interrupt delivery.
    unsafe { riscv::register::sstatus::clear_sie() };
}

/// @description 在即将执行 noreturn context transfer 前永久关闭 local interrupts。
pub(crate) fn disable_for_transfer() {
    // SAFETY: caller never returns to a Rust frame that expects the previous interrupt state.
    unsafe { riscv::register::sstatus::clear_sie() };
}

/// @description 等待下一次 local interrupt。
#[inline(always)]
pub(crate) fn wait_for_interrupt() {
    riscv::asm::wfi();
}
