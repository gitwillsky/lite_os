use core::arch::naked_asm;

use super::startup;

// SAFETY: entry.rs defines both symbols with the raw AArch64 boot-register callback ABI.
unsafe extern "C" {
    fn __liteos_primary_entry(hardware_cpu: usize, platform_opaque: usize) -> !;
    fn __liteos_secondary_entry(hardware_cpu: usize, platform_opaque: usize) -> !;
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[cfg_attr(target_os = "none", unsafe(link_section = ".text.entry"))]
// SAFETY: Linux arm64 Image enters with x0=DTB physical address and MMU off. This low-LMA stub
// touches no Rust storage: it enters EL1, installs temporary identity TTBR0 plus high direct-map
// TTBR1, enables stage-1 translation, and branches indirectly to the linked high-half entry.
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
        b       1f
        .long   0
        .quad   0x80000
        .quad   kernel_image_end_phys - 0x40080000
        .quad   0
        .quad   0
        .quad   0
        .quad   0
        .long   0x644d5241
        .long   0
1:
        mov     x21, x0
        mrs     x20, mpidr_el1
        mov     x9, #0xffff
        movk    x9, #0x00ff, lsl #16
        movk    x9, #0x00ff, lsl #32
        and     x20, x20, x9

        mrs     x9, CurrentEL
        cmp     x9, #8
        b.ne    2f
        mov     x9, #(1 << 31)
        msr     hcr_el2, x9
        mov     x9, #3
        msr     cnthctl_el2, x9
        msr     cntvoff_el2, xzr
        msr     sctlr_el1, xzr
        adr     x9, 2f
        msr     elr_el2, x9
        mov     x9, #0x3c5
        msr     spsr_el2, x9
        eret
2:
        mrs     x9, CurrentEL
        cmp     x9, #4
        b.ne    9f
        msr     daifset, #0xf
        msr     spsel, #1

        adrp    x9, __boot_ttbr0
        add     x9, x9, :lo12:__boot_ttbr0
        msr     ttbr0_el1, x9
        adrp    x9, __boot_ttbr1
        add     x9, x9, :lo12:__boot_ttbr1
        msr     ttbr1_el1, x9
        mov     x9, #0xff
        msr     mair_el1, x9

        mrs     x11, id_aa64mmfr0_el1
        and     x9, x11, #0xf
        mov     x10, #5
        cmp     x9, x10
        csel    x9, x9, x10, ls
        lsl     x9, x9, #32
        mov     x10, #0x3519
        movk    x10, #0xb519, lsl #16
        orr     x10, x10, x9
        ubfx    x11, x11, #4, #4
        cmp     x11, #2
        b.ne    3f
        mov     x11, #1
        lsl     x11, x11, #36
        orr     x10, x10, x11
3:
        msr     tcr_el1, x10
        isb

        mrs     x9, sctlr_el1
        orr     x9, x9, #1
        orr     x9, x9, #(1 << 2)
        orr     x9, x9, #(1 << 12)
        orr     x9, x9, #(1 << 3)
        orr     x9, x9, #(1 << 4)
        msr     sctlr_el1, x9
        isb
        ldr     x9, 8f
        br      x9
        .balign 8
8:
        .quad   __liteos_high_entry
9:
        msr     daifset, #0xf
10:
        wfi
        b       10b

        .balign 4096
__boot_ttbr0:
        .quad   0x0060000000000405
        .quad   0x0040000040000701
        .zero   510 * 8

        .balign 4096
__boot_ttbr1:
        .zero   256 * 8
        .quad   0x0060000000000405
        .set    boot_pa, 0x40000000
        .rept   255
        .quad   boot_pa | 0x0040000000000701
        .set    boot_pa, boot_pa + 0x40000000
        .endr
        "
    )
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
// SAFETY: the low stub has enabled TTBR1 and preserves x20=MPIDR identity/x21=DTB PA. This entry
// selects an owned high-half stack before calling Rust; startup topology is release-published
// before firmware starts any secondary CPU.
unsafe extern "C" fn __liteos_high_entry() -> ! {
    naked_asm!(
        "
        .arch_extension pan
        msr     pan, #1
        adrp    x9, {table_address}
        add     x9, x9, :lo12:{table_address}
        ldr     x10, [x9]
        mov     x11, -1
        cmp     x10, x11
        b.ne    1f
        adrp    x9, boot_stack_top
        add     sp, x9, :lo12:boot_stack_top
        msr     tpidr_el1, x20
        mov     x0, x20
        mov     x1, x21
        bl      {clear_bss}
        mov     x0, x20
        mov     x1, x21
        bl      {primary}
1:
        dmb     ishld
        adrp    x9, {table_length}
        add     x9, x9, :lo12:{table_length}
        ldr     x11, [x9]
2:
        cbz     x11, 9f
        ldr     x12, [x10, #{hardware_id_offset}]
        cmp     x12, x20
        b.eq    3f
        add     x10, x10, #{entry_size}
        sub     x11, x11, #1
        b       2b
3:
        ldr     x12, [x10, #{logical_id_offset}]
        ldr     x13, [x10, #{stack_top_offset}]
        mov     sp, x13
        msr     tpidr_el1, x12
        mov     x0, x20
        mov     x1, x21
        bl      {secondary}
9:
        msr     daifset, #0xf
10:
        wfi
        b       10b
        ",
        table_address = sym startup::TABLE_ADDRESS,
        table_length = sym startup::TABLE_LENGTH,
        hardware_id_offset = const startup::HARDWARE_ID_OFFSET,
        logical_id_offset = const startup::LOGICAL_ID_OFFSET,
        stack_top_offset = const startup::STACK_TOP_OFFSET,
        entry_size = const startup::ENTRY_SIZE,
        clear_bss = sym clear_bss,
        primary = sym __liteos_primary_entry,
        secondary = sym __liteos_secondary_entry,
    )
}

/// Return the low physical PSCI CPU_ON entry shared by all secondary CPUs.
pub(crate) fn entry_address() -> usize {
    0x4008_0000
}

extern "C" fn clear_bss() {
    // SAFETY: aarch64.ld defines these address-only boundaries around the writable high-half BSS.
    unsafe extern "C" {
        fn sbss();
        fn ebss();
    }
    // SAFETY: the primary uniquely owns BSS before publication and TTBR1 maps this complete range.
    unsafe {
        let start = sbss as *const () as usize;
        let end = ebss as *const () as usize;
        if end > start {
            core::ptr::write_bytes(start as *mut u8, 0, end - start);
        }
    }
}
