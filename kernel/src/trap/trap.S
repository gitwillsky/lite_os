.altmacro

.macro SAVE_GP idx
    sd x\idx, \idx*8(sp)
.endm

.macro LOAD_GP idx
    ld x\idx, \idx*8(sp)
.endm

.globl __alltraps

# RISC-V 要求陷阱处理程序至少2字节对齐，通常是4字节对齐。
# .align 2 表示对齐到 2^2 = 4 字节边界。
# 这确保了地址的低两位为0，符合直接模式对 stvec.BASE 的要求。
.align 2
__alltraps:
    # 如果从 U-mode 进入，sscratch 通常保存内核栈顶，sp 是用户栈顶
    # 如果从 S-mode 进入，sscratch 可能有其他用途或为0，sp 是内核栈
    csrrw sp, sscratch, sp # sscratch 代表内核栈，sp 代表用户栈，这里做交换
    addi sp, sp, -34*8     # allocate a TrapContext on kernel stack

    sd x1, 1*8(sp)
    # skip sp(x2), we will save it later
    sd x3, 3*8(sp)
    # 跳过线程指针寄存器 tp(x4)，用户空间不会用到它

    # save x5 ~ x31
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr

    # save sstatus、sepc、sscratch
    csrr t0, sstatus
    csrr t1, sepc
    csrr t2, sscratch

    sd t0, 32*8(sp)
    sd t1, 33*8(sp)
    sd t2, 2*8(sp) # 保存用户栈顶

    mv a0, sp
    call trap_handler

__restore:
    mv sp, a0

    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    ld t2, 2*8(sp)

    csrw sstatus, t0
    csrw sepc, t1
    csrw sscratch, t2

    ld x1, 1*8(sp)
    ld x3, 3*8(sp)

    .set m, 5
    .rept 27
        LOAD_GP %m
        .set m, m+1
    .endr

    # release TrapContext on kernel stack
    addi sp, sp, 34*8
    # swap user stack and kernel stack
    csrrw sp, sscratch, sp

    # return to userland
    sret
