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
    // SSIP is the durable deferred-work token published by the external handler. Bootstrap owns
    // this temporary source enable because the scheduler has not reached its permanent enable seam.
    unsafe { riscv::register::sie::set_ssoft() };
    // SAFETY: trap.S temporarily owns local SIE and its trap-entry resume fixup; caller established
    // the trap/PLIC initialization and bootstrap source constraints documented above.
    wait_once_with_local_irq_masked();
    // SAFETY: the bootstrap assertion above proved SSIE was disabled on entry.
    unsafe { riscv::register::sie::clear_ssoft() };
    if !external_enabled {
        // SAFETY: kernel restores only the calling CPU's previously disabled SEIE source.
        unsafe { riscv::register::sie::clear_sext() };
    }
    // SAFETY: local was captured on this CPU and no context switch occurs in bootstrap wait.
    unsafe { restore_local(local) };
}

/// @description 在 caller 持有 local IRQ guard 时原子等待一次 interrupt。
///
/// assembly 临时打开 SIE，以固定 WFI/resume PC 关闭 enable-to-WFI 丢边沿窗口，返回前再次
/// 关闭 SIE。interrupt source mask 保持不变，caller 可在 guard 恢复旧状态前重新检查调度状态。
///
/// @return 一次 WFI 返回后无返回值，local SIE 仍关闭。
/// @errors caller 必须已在当前 CPU 关闭 local SIE，且不得在 assembly seam 内发生 context switch。
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

/// @description 启用 calling CPU 的 supervisor timer interrupt source。
///
/// @return 无返回值。
/// @errors 首个 timer deadline 必须在 global interrupt enable 前完成 programming。
// SAFETY: caller must program the first deadline before enabling global delivery.
pub(crate) unsafe fn enable_timer_source() {
    // SAFETY: caller owns local timer initialization ordering.
    unsafe { riscv::register::sie::set_stimer() };
}

/// @description 在 calling CPU 保持 scheduler idle 时屏蔽 supervisor timer source。
///
/// absolute deadline 仍由 timer module 独占；恢复运行 task 前会先写入新 deadline。
/// @return 无返回值。
/// @errors caller 必须持有当前 CPU 的 local IRQ guard。
pub(crate) fn disable_timer_source() {
    // SAFETY: scheduler idle owns this CPU's STIE source and local SIE is already disabled.
    unsafe { riscv::register::sie::clear_stimer() };
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
