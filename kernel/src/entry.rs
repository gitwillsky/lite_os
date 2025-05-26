use core::arch::naked_asm;

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
            la sp, boot_stack_top
            add t0, x10, x0
            add t1, x11, x0
            call {clear_bss}
            mv x10, t0
            mv x11, t1
            call kmain
        1:
            wfi
            j 1b
        ",
        clear_bss = sym clear_bss,
    )
}

extern "C" fn clear_bss() {
    unsafe extern "C" {
        static mut sbss: u8;
        static mut ebss: u8;
    }
    unsafe {
        let start = sbss as *const u8 as usize;
        let end = ebss as *const u8 as usize;
        let count = end - start;
        if count > 0 {
            core::ptr::write_bytes(sbss as *mut u8, 0, count);
        }
    }
}
