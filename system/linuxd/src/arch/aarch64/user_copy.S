.section .text.linuxd_user_copy,"ax"
.p2align 2
.global linuxd_user_copy
.global linuxd_user_copy_end
.global linuxd_user_copy_fault
.type linuxd_user_copy,%function
linuxd_user_copy:
    // 仅对齐可能故障的应用侧，确保单次 8 字节访问不跨页。
    cmp x3, #0
    csel x4, x1, x0, eq
1:
    cbz x2, 5f
    tst x4, #7
    b.eq 2f
    ldrb w5, [x1], #1
    strb w5, [x0], #1
    add x4, x4, #1
    sub x2, x2, #1
    b 1b
2:
    cmp x2, #8
    b.lo 4f
    ldr x5, [x1], #8
    str x5, [x0], #8
    sub x2, x2, #8
    b 2b
4:
    cbz x2, 5f
    ldrb w5, [x1], #1
    strb w5, [x0], #1
    sub x2, x2, #1
    b 4b
5:
    mov x0, #0
    ret
linuxd_user_copy_end:
linuxd_user_copy_fault:
    mov x0, #1
    ret
.size linuxd_user_copy, .-linuxd_user_copy
