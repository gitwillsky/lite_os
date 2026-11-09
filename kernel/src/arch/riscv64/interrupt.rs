//! RISC-V supervisor local interrupt mechanism。

/// 初始化本 CPU 的 supervisor IMSIC 文件；调用者已关闭 SIE，ids 为 DTB 验证的 63..=2047。
pub(crate) fn initialize_messages(ids: u32) {
    message_write(0x70, 0);
    message_write(0x72, 1);
    for word in 0..=(ids / 64) {
        message_write(0x80 + word as usize * 2, 0);
        message_write(0xc0 + word as usize * 2, 0);
    }
    message_write(0x72, 0);
    message_write(0x70, 1);
}

/// 改变本 CPU 的 EIE；selector/ireg 序列只能在 SIE 关闭时使用。
pub(crate) fn enable_message(id: u32, enabled: bool) {
    let selector = 0xc0 + (id / 64) as usize * 2;
    let bit = 1usize << (id % 64);
    // SAFETY: 调用方持有当前 CPU 的 IRQ guard；0x150/0x151 为 Ssaia 的 SISELECT/SIREG。
    unsafe {
        if enabled {
            core::arch::asm!("csrw 0x150, {selector}", "csrs 0x151, {bit}", selector = in(reg) selector, bit = in(reg) bit, options(nostack));
        } else {
            core::arch::asm!("csrw 0x150, {selector}", "csrc 0x151, {bit}", selector = in(reg) selector, bit = in(reg) bit, options(nostack));
        }
    }
}

/// 原子领取并清除本 CPU 的一个 EIP 位；调用方已关闭 SIE。
pub(crate) fn take_message_pending(id: u32) -> bool {
    let selector = 0x80 + (id / 64) as usize * 2;
    let bit = 1usize << (id % 64);
    let previous: usize;
    // SAFETY: 当前 CPU 独占 selector/ireg 序列；CSRR[C] 仅清除授权的单一 pending 位。
    unsafe {
        core::arch::asm!("csrw 0x150, {selector}", "csrrc {previous}, 0x151, {bit}", selector = in(reg) selector, previous = out(reg) previous, bit = in(reg) bit, options(nostack));
    }
    previous & bit != 0
}

/// 原子 claim 当前 CPU 最高优先级消息并清除该 EIP；0 表示没有 eligible 消息，无另一次 EOI。
pub(crate) fn claim_message() -> u32 {
    let top: usize;
    // SAFETY: hardirq 的本地 SIE 已关闭；STOPEI 的 swap-to-zero 是标准 IMSIC claim 操作。
    unsafe {
        core::arch::asm!("csrrw {top}, 0x15c, zero", top = out(reg) top, options(nostack));
    }
    (top >> 16) as u32
}

fn message_write(selector: usize, value: usize) {
    // SAFETY: 仅在本地 IMSIC 初始化、SIE 关闭时调用，写入已验证 interrupt-file 的间接寄存器。
    unsafe {
        core::arch::asm!("csrw 0x150, {selector}", "csrw 0x151, {value}", selector = in(reg) selector, value = in(reg) value, options(nostack));
    }
}

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
