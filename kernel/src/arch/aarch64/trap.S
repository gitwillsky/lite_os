    .arch armv8-a+simd
    .macro VECTOR target
        b \target
        .balign 128
    .endm

    .section .text.trampoline
    .align 11
    .global __aarch64_vectors
__aarch64_vectors:
    VECTOR __aarch64_kernel_sync
    VECTOR __aarch64_kernel_irq
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_kernel_sync
    VECTOR __aarch64_kernel_irq
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_user_sync
    VECTOR __aarch64_user_irq
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported
    VECTOR __aarch64_unsupported

__aarch64_user_sync:
    // SP_EL1 remains on the task's TTBR1 kernel stack while EL0 runs. Keep the three scratch
    // registers in one aligned, short-lived stack slot until their UserContext fields are stored.
    sub sp, sp, #32
    stp x9, x10, [sp, #0]
    str x11, [sp, #16]
    mrs x9, sp_el0
    mrs x10, tpidr_el0
    mov x11, #0
    msr sp_el0, x11
    b __aarch64_user_common
__aarch64_user_irq:
    sub sp, sp, #32
    stp x9, x10, [sp, #0]
    str x11, [sp, #16]
    mrs x9, sp_el0
    mrs x10, tpidr_el0
    mov x11, #1
    msr sp_el0, x11
__aarch64_user_common:
    // The interrupted SP_EL1 is the reserved page start and UserContext lives at its fixed +16
    // offset. The 32-byte scratch reservation makes that address current sp+48, with no pointer
    // load, per-task metadata, or user-visible system register borrowed by the trap mechanism.
    add x11, sp, #48
    stp x0, x1, [x11, #0]
    stp x2, x3, [x11, #16]
    stp x4, x5, [x11, #32]
    stp x6, x7, [x11, #48]
    str x8, [x11, #64]
    str x9, [x11, #248]
    str x10, [x11, #304]
    ldp x9, x10, [sp, #0]
    stp x9, x10, [x11, #72]
    ldr x9, [sp, #16]
    str x9, [x11, #88]
    add sp, sp, #32
    stp x12, x13, [x11, #96]
    stp x14, x15, [x11, #112]
    stp x16, x17, [x11, #128]
    stp x18, x19, [x11, #144]
    stp x20, x21, [x11, #160]
    stp x22, x23, [x11, #176]
    stp x24, x25, [x11, #192]
    stp x26, x27, [x11, #208]
    stp x28, x29, [x11, #224]
    str x30, [x11, #240]
    mrs x10, elr_el1
    str x10, [x11, #256]
    mrs x10, spsr_el1
    str x10, [x11, #264]
    // Vector state remains live and is owned exclusively by KernelContext on a real task switch.
    // Ordinary traps close FPEN before entering Rust without copying q0-q31.
    mrs x10, cpacr_el1
    bic x10, x10, #(3 << 20)
    msr cpacr_el1, x10
    isb
    ldr x12, [x11, #280]
    ldr x13, [x11, #288]
    ldr x14, [x11, #296]
    mov sp, x12
    msr tpidr_el1, x14
    br x13

__aarch64_kernel_sync:
    sub sp, sp, #272
    stp x8, x9, [sp, #64]
    mov x9, #0
    b __aarch64_kernel_common
__aarch64_kernel_irq:
    sub sp, sp, #272
    stp x8, x9, [sp, #64]
    // If the IRQ arrived after a guarded wait unmasked DAIF.I but before WFI retired,
    // ELR points at that WFI. The controller handler consumes the sole device edge, so return
    // directly to the mask-and-return label instead of sleeping again with no future wakeup.
    mrs x8, elr_el1
    adr x9, __local_irq_wait_wfi
    cmp x8, x9
    b.ne 1f
    adr x8, __local_irq_wait_wfi_resume
    msr elr_el1, x8
1:
    mov x9, #1
__aarch64_kernel_common:
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x10, sp_el0
    str x10, [sp, #248]
    mov x10, x9
    msr sp_el0, x10
    bl __liteos_kernel_trap
    ldr x10, [sp, #248]
    msr sp_el0, x10
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #272
    eret

__aarch64_unsupported:
    msr daifset, #0xf
1:  wfi
    b 1b

    .global __aarch64_restore
__aarch64_restore:
    // x0=context VA, x1=tagged TTBR0, x2=destination low vector VA. This entry executes through
    // permanent TTBR1 until the destination TTBR0 makes the low context/vector mappings valid.
    dsb ishst
    msr ttbr0_el1, x1
    isb
    msr vbar_el1, x2
    isb
    mov x16, x0
    ldr x9, [x16, #280]
    mov sp, x9
    ldr x9, [x16, #256]
    msr elr_el1, x9
    ldr x9, [x16, #264]
    msr spsr_el1, x9
    ldr x9, [x16, #248]
    msr sp_el0, x9
    ldr x9, [x16, #304]
    msr tpidr_el0, x9
    msr tpidrro_el0, xzr
    // Kernel Rust runs with FPEN closed. Reopen it for EL0 without touching the live vector file;
    // KernelContext switch is the sole owner that may have replaced that file meanwhile.
    mrs x11, cpacr_el1
    orr x11, x11, #(3 << 20)
    msr cpacr_el1, x11
    isb
    ldp x0, x1, [x16, #0]
    ldp x2, x3, [x16, #16]
    ldp x4, x5, [x16, #32]
    ldp x6, x7, [x16, #48]
    ldp x8, x9, [x16, #64]
    ldp x10, x11, [x16, #80]
    ldp x12, x13, [x16, #96]
    ldp x14, x15, [x16, #112]
    ldp x18, x19, [x16, #144]
    ldp x20, x21, [x16, #160]
    ldp x22, x23, [x16, #176]
    ldp x24, x25, [x16, #192]
    ldp x26, x27, [x16, #208]
    ldp x28, x29, [x16, #224]
    ldr x30, [x16, #240]
    ldr x17, [x16, #136]
    ldr x16, [x16, #128]
    eret

    .align 4
    .global __signal_trampoline
__signal_trampoline:
    mov x8, #139
    svc #0
    brk #0

    .section .text
    .align 2
    .global __wait_with_local_irq_masked
    .global __local_irq_wait_wfi
    .global __local_irq_wait_wfi_resume
__wait_with_local_irq_masked:
    msr daifclr, #2
__local_irq_wait_wfi:
    wfi
__local_irq_wait_wfi_resume:
    msr daifset, #2
    ret
