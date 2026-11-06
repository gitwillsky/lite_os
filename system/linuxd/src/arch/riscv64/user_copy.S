.section .text.linuxd_user_copy,"ax"
.p2align 2
.global linuxd_user_copy
.global linuxd_user_copy_end
.global linuxd_user_copy_fault
.type linuxd_user_copy,@function
linuxd_user_copy:
    # RISC-V 不要求支持非对齐字访问，两侧偏移相同才可以共同对齐。
    xor t0, a0, a1
    andi t0, t0, 7
    bnez t0, 4f
1:
    beqz a2, 5f
    andi t0, a1, 7
    beqz t0, 2f
    lbu t1, 0(a1)
    sb t1, 0(a0)
    addi a1, a1, 1
    addi a0, a0, 1
    addi a2, a2, -1
    j 1b
2:
    li t0, 8
    bltu a2, t0, 4f
    ld t1, 0(a1)
    sd t1, 0(a0)
    addi a1, a1, 8
    addi a0, a0, 8
    addi a2, a2, -8
    j 2b
4:
    beqz a2, 5f
    lbu t1, 0(a1)
    sb t1, 0(a0)
    addi a1, a1, 1
    addi a0, a0, 1
    addi a2, a2, -1
    j 4b
5:
    li a0, 0
    ret
linuxd_user_copy_end:
linuxd_user_copy_fault:
    li a0, 1
    ret
.size linuxd_user_copy, .-linuxd_user_copy
