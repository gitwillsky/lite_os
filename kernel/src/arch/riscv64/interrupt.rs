//! RISC-V supervisor local interrupt mechanism。

/// 调用前的 local interrupt enable 状态。
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalInterruptState {
    enabled: bool,
}

/// 关闭 calling CPU 的 supervisor interrupt 并保存原状态。
///
/// # Returns
///
/// 只能用于同一 CPU 恢复的 opaque 状态。
///
/// # Errors
///
/// 必须在 S-mode kernel context 调用。
#[inline(always)]
pub(crate) fn disable_local() -> LocalInterruptState {
    let enabled = riscv::register::sstatus::read().sie();
    // SAFETY: kernel runs in S-mode and updates only the calling CPU's SIE bit.
    unsafe { riscv::register::sstatus::clear_sie() };
    LocalInterruptState { enabled }
}

/// 恢复 calling CPU 的 local interrupt enable 状态。
///
/// # Parameters
///
/// - `state`: 同一 CPU 上由 `disable_local` 取得的 opaque 状态。
///
/// # Errors
///
/// 跨 CPU 使用会破坏 local interrupt invariant；RAII guard 从类型层禁止移动。
#[inline(always)]
// SAFETY: caller must return the opaque state to the same CPU that produced it.
pub(crate) unsafe fn restore_local(state: LocalInterruptState) {
    if state.enabled {
        // SAFETY: caller proves that `state` belongs to this CPU and only local SIE is updated.
        unsafe { riscv::register::sstatus::set_sie() };
    }
}

/// 启用 scheduler 运行所需的 supervisor software/external/global interrupts。
///
/// # Errors
///
/// local trap vector 与 interrupt controller 必须已初始化。
// SAFETY: caller must establish the documented local initialization order.
pub(crate) unsafe fn enable_scheduler_interrupts() {
    // SAFETY: caller establishes the initialization ordering documented above.
    unsafe {
        riscv::register::sie::set_ssoft();
        riscv::register::sie::set_sext();
        riscv::register::sstatus::set_sie();
    }
}

/// 在 caller 持有 local IRQ guard 时原子等待一次 interrupt。
///
/// assembly 临时打开 SIE，以固定 WFI/resume PC 关闭 enable-to-WFI 丢边沿窗口，返回前再次
/// 关闭 SIE。interrupt source mask 保持不变，caller 可在 guard 恢复旧状态前重新检查调度状态。
///
/// # Returns
///
/// 一次 WFI 返回后无返回值，local SIE 仍关闭。
///
/// # Errors
///
/// caller 必须已在当前 CPU 关闭 local SIE，且不得在 assembly seam 内发生 context switch。
pub(crate) fn wait_with_local_irq_masked() {
    wait_once_with_local_irq_masked();
}

#[inline(always)]
fn wait_once_with_local_irq_masked() {
    // SAFETY: 此 symbol 由 trap.S 实现，保持 Rust ABI，并只临时修改 calling CPU 的 SIE。
    unsafe extern "C" {
        fn __wait_with_local_irq_masked();
    }
    // SAFETY: caller 保证 local SIE 已关闭；assembly 只临时开 SIE 并在返回前再次关闭。
    unsafe { __wait_with_local_irq_masked() };
}

/// 启用 calling CPU 的 supervisor timer interrupt source。
///
/// # Errors
///
/// 首个 timer deadline 必须在 global interrupt enable 前完成 programming。
// SAFETY: caller must program the first deadline before enabling global delivery.
pub(crate) unsafe fn enable_timer_source() {
    // SAFETY: caller owns local timer initialization ordering.
    unsafe { riscv::register::sie::set_stimer() };
}

/// 在 calling CPU 保持 scheduler idle 时屏蔽 supervisor timer source。
///
/// absolute deadline 仍由 timer module 独占；恢复运行 task 前会先写入新 deadline。
///
/// # Errors
///
/// caller 必须持有当前 CPU 的 local IRQ guard。
pub(crate) fn disable_timer_source() {
    // SAFETY: scheduler idle owns this CPU's STIE source and local SIE is already disabled.
    unsafe { riscv::register::sie::clear_stimer() };
}

/// 发布 calling CPU 的 software interrupt pending bit。
#[inline(always)]
pub(crate) fn raise_software() {
    // SAFETY: kernel sets only the calling CPU's supervisor software pending bit.
    unsafe { riscv::register::sip::set_ssoft() }
}

/// 清除 calling CPU 的 software interrupt pending bit。
#[inline(always)]
pub(crate) fn clear_software() {
    // SAFETY: kernel clears only the calling CPU's supervisor software pending bit.
    unsafe { riscv::register::sip::clear_ssoft() }
}

/// 关闭 local interrupts，用于 panic fail-stop 路径。
pub(crate) fn disable_for_fail_stop() {
    // SAFETY: panic handling never restores local interrupt delivery.
    unsafe { riscv::register::sstatus::clear_sie() };
}

/// 在即将执行 noreturn context transfer 前永久关闭 local interrupts。
pub(crate) fn disable_for_transfer() {
    // SAFETY: caller never returns to a Rust frame that expects the previous interrupt state.
    unsafe { riscv::register::sstatus::clear_sie() };
}

/// 等待下一次 local interrupt。
#[inline(always)]
pub(crate) fn wait_for_interrupt() {
    riscv::asm::wfi();
}
