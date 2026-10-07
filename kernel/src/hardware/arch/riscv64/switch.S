.altmacro
.option arch, +d
.macro SAVE_SN n
    sd s\n, (\n+2)*8(a0)
.endm
.macro LOAD_SN n
    ld s\n, (\n+2)*8(a1)
.endm
.macro SAVE_FSN n
    fsd fs\n, (\n+14)*8(a0)
.endm
.macro LOAD_FSN n
    fld fs\n, (\n+14)*8(a1)
.endm
    .section .text
    .globl __switch

__switch:
    # __switch(
    #     current_task_cx_ptr: *mut TaskContext,
    #     next_task_cx_ptr: *const TaskContext
    # )
    # save kernel stack of current task
    sd sp, 8(a0)
    # save ra & s0~s11 of current execution
    sd ra, 0(a0)
    .set n, 0
    .rept 12
        SAVE_SN %n
        .set n, n + 1
    .endr
    # LP64D requires fs0~fs11 and fcsr to survive a kernel context switch.
    .set n, 0
    .rept 12
        SAVE_FSN %n
        .set n, n + 1
    .endr
    frcsr t0
    sd t0, 26*8(a0)
    # restore ra & s0~s11 of next execution
    ld ra, 0(a1)
    .set n, 0
    .rept 12
        LOAD_SN %n
        .set n, n + 1
    .endr
    ld t0, 26*8(a1)
    fscsr t0
    .set n, 0
    .rept 12
        LOAD_FSN %n
        .set n, n + 1
    .endr
    # restore kernel stack of next task
    ld sp, 8(a1)
    ret
