.altmacro
.option arch, +d
.macro SAVE_GP n
    sd x\n, \n*8(sp)
.endm
.macro LOAD_GP n
    ld x\n, \n*8(sp)
.endm
.macro SAVE_USER_FP n
    fsd f\n, (39+\n)*8(sp)
.endm
.macro LOAD_USER_FP n
    fld f\n, (39+\n)*8(sp)
.endm
.macro SAVE_KERNEL_FP n
    fsd f\n, (32+\n)*8(sp)
.endm
.macro LOAD_KERNEL_FP n
    fld f\n, (32+\n)*8(sp)
.endm
    .section .text.trampoline
    .globl __alltraps
    .globl __restore
    .globl __kernel_trap
    .globl __signal_trampoline
    .align 2
__alltraps:
    csrrw sp, sscratch, sp
    # now sp->*TrapContext in user space, sscratch->user stack
    # save other general purpose registers
    sd x1, 1*8(sp)
    # skip sp(x2), we will save it later
    sd x3, 3*8(sp)
    sd x4, 4*8(sp)
    # save x5~x31
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    # we can use t0/t1/t2 freely, because they have been saved in TrapContext
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)
    sd t1, 33*8(sp)
    # FS=Dirty means the live register file is newer than the context image. Clean/Initial
    # already have an authoritative memory/zero image and must stay off the integer trap path.
    li t2, (3 << 13)
    and t2, t0, t2
    li t1, (3 << 13)
    bne t2, t1, 1f
    call __save_user_fp
    li t2, (3 << 13)
    not t2, t2
    and t0, t0, t2
    li t2, (2 << 13)
    or t0, t0, t2
    sd t0, 32*8(sp)
1:
    # User FS ownership is now frozen in TrapContext. Kernel code follows the LP64D psABI and
    # both kernel-trap entry and context switch save FP registers, so live FS must be Dirty before
    # entering any Rust/kernel continuation. Leaving the user's Off state live recursively traps
    # on the first kernel FP save and makes the CPU fail silently.
    li t2, (3 << 13)
    csrs sstatus, t2
    # read user stack from sscratch and save it in TrapContext
    csrr t2, sscratch
    sd t2, 2*8(sp)
    # load kernel_satp into t0
    ld t0, 34*8(sp)
    # load trap_handler into t1
    ld t1, 36*8(sp)
    # user gp/tp have been saved; install the current kernel execution identity
    ld tp, 37*8(sp)
    ld gp, 38*8(sp)
    # move to kernel_sp
    ld sp, 35*8(sp)
    # switch to kernel space
    csrw satp, t0
    # jump to trap_handler
    jr t1

    # ----------------------------------------------------------------------
    # Kernel-mode trap entry
    # Save minimal caller-saved state, call into Rust to handle, then sret
    # This path is used when traps/interrupts occur while executing in kernel
    # mode (e.g., idle loop). It must not corrupt kernel registers/state.
    # ----------------------------------------------------------------------
    .align 2
__kernel_trap:
    # Save all asynchronously clobberable integer/FP state. 66 words preserves 16-byte stack alignment.
    addi sp, sp, -(66*8)
    sd x1, 1*8(sp)
    # skip sp(x2)
    sd x3, 3*8(sp)
    # skip tp(x4)
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    .set n, 0
    .rept 32
        SAVE_KERNEL_FP %n
        .set n, n+1
    .endr
    frcsr t0
    sd t0, 64*8(sp)
    # If an external IRQ arrived after bootstrap enabled SIE but before WFI executed, sepc points
    # at WFI. Resume after it so the already-acknowledged sole device edge cannot be followed by
    # an unbounded sleep. Exact PC identity avoids a flag or a second interrupt-delivery path.
    csrr t0, sepc
    la t1, __bootstrap_external_wfi
    bne t0, t1, 9f
    la t0, __bootstrap_external_wfi_resume
    csrw sepc, t0
9:
    call __liteos_kernel_trap
    ld t0, 64*8(sp)
    fscsr t0
    .set n, 0
    .rept 32
        LOAD_KERNEL_FP %n
        .set n, n+1
    .endr
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    addi sp, sp, (66*8)
    sret

    .section .text
    .align 2
    .globl __wait_for_external_interrupt
    .globl __bootstrap_external_wfi
    .globl __bootstrap_external_wfi_resume
__wait_for_external_interrupt:
    csrsi sie, 2
    csrsi sstatus, 2
__bootstrap_external_wfi:
    wfi
__bootstrap_external_wfi_resume:
    csrci sstatus, 2
    csrci sie, 2
    ret

    .section .text.trampoline
__restore:
    # a0: *TrapContext in user space(Constant); a1: user space token
    # 先切到新页表中确定存在的 TrapContext 栈，避免 satp 切换后仍持有仅内核页表可见的栈。
    csrw sscratch, a0
    mv sp, a0
    csrw satp, a1
    # now sp points to TrapContext in user space, start restoring based on it
    # restore sstatus/sepc
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    csrw sepc, t1
    # Off is the zero-cost integer state. Initial receives a zero register file without reading
    # stale physical-register contents; Clean/Dirty restore the authoritative memory image.
    li t2, (3 << 13)
    and t2, t0, t2
    beqz t2, 2f
    li t1, (1 << 13)
    beq t2, t1, 1f
    csrw sstatus, t0
    call __restore_user_fp
    j 2f
1:
    csrw sstatus, t0
    call __initialize_user_fp
2:
    # FP helper instructions may mark FS Dirty; publish the logical saved state for sret.
    ld t0, 32*8(sp)
    csrw sstatus, t0
    # restore general purpose registers except x0/sp/tp
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    # tp must be the user's TLS pointer at the exact point of sret; no kernel code runs afterwards
    ld tp, 4*8(sp)
    # back to user stack
    ld sp, 2*8(sp)
    sret

    .align 2
__signal_trampoline:
    li a7, 139
    ecall
    unimp

    .align 2
__save_user_fp:
    .set n, 0
    .rept 32
        SAVE_USER_FP %n
        .set n, n+1
    .endr
    frcsr t2
    sd t2, 71*8(sp)
    ret

    .align 2
__restore_user_fp:
    .set n, 0
    .rept 32
        LOAD_USER_FP %n
        .set n, n+1
    .endr
    ld t2, 71*8(sp)
    fscsr t2
    ret

.macro INITIALIZE_USER_FP n
    fmv.d.x f\n, zero
.endm
    .align 2
__initialize_user_fp:
    .set n, 0
    .rept 32
        INITIALIZE_USER_FP %n
        .set n, n+1
    .endr
    fscsr zero
    ret
