
    .align 3
    .section .data
    .global _num_app
_num_app:
    .quad 4
    .quad app_0_start
    .quad app_1_start
    .quad app_2_start
    .quad app_3_start
    .quad app_3_end

    .global _app_names
_app_names:
    .string "hello"
    .string "initproc"
    .string "shutdown"
    .string "user_shell"

    .section .data
    .global app_0_start
    .global app_0_end
    .align 3
app_0_start:
    .incbin "target/riscv64gc-unknown-none-elf/release/hello"
app_0_end:

    .section .data
    .global app_1_start
    .global app_1_end
    .align 3
app_1_start:
    .incbin "target/riscv64gc-unknown-none-elf/release/initproc"
app_1_end:

    .section .data
    .global app_2_start
    .global app_2_end
    .align 3
app_2_start:
    .incbin "target/riscv64gc-unknown-none-elf/release/shutdown"
app_2_end:

    .section .data
    .global app_3_start
    .global app_3_end
    .align 3
app_3_start:
    .incbin "target/riscv64gc-unknown-none-elf/release/user_shell"
app_3_end:
