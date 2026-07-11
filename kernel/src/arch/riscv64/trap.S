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
    # eager-save all user floating-point state before kernel code can use it
    .set n, 0
    .rept 32
        SAVE_USER_FP %n
        .set n, n+1
    .endr
    frcsr t0
    sd t0, 71*8(sp)
    # we can use t0/t1/t2 freely, because they have been saved in TrapContext
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)
    sd t1, 33*8(sp)
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
    sfence.vma
    # jump to trap_handler
    jr t1

    # ----------------------------------------------------------------------
    # Kernel-mode trap entry
    # Save minimal caller-saved state, call into Rust to handle, then sret
    # This path is used when traps/interrupts occur while executing in kernel
    # mode (e.g., idle loop). It must not corrupt kernel registers/state.
    # ----------------------------------------------------------------------
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
    call rust_trap_from_kernel
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

__restore:
    # a0: *TrapContext in user space(Constant); a1: user space token
    # 先切到新页表中确定存在的 TrapContext 栈，避免 satp 切换后仍持有仅内核页表可见的栈。
    csrw sscratch, a0
    mv sp, a0
    csrw satp, a1
    sfence.vma
    # now sp points to TrapContext in user space, start restoring based on it
    # restore sstatus/sepc
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    csrw sstatus, t0
    csrw sepc, t1
    # sstatus.FS is now enabled, so restore the complete user FP state and fcsr
    .set n, 0
    .rept 32
        LOAD_USER_FP %n
        .set n, n+1
    .endr
    ld t0, 71*8(sp)
    fscsr t0
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
