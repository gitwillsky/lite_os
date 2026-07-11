use core::{
    arch::naked_asm,
    sync::atomic::{AtomicUsize, Ordering},
};

const BSS_PENDING: usize = 0x4253_5350;
const BSS_READY: usize = 0x4253_5352;

// 该标志使用非零初值以确保位于 .data；若放在尚未清零的 BSS，次核可能误判初始化已完成。
static BSS_STATE: AtomicUsize = AtomicUsize::new(BSS_PENDING);

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
            # 1. 每个 hart 使用 linker 预留的独立 128 KiB 启动栈。
            slli t0, a0, 17
            la sp, boot_stack_top
            sub sp, sp, t0

            # 2. TP 是内核中 hart 身份的唯一来源；sscratch 在 trap 初始化前保持为零。
            mv tp, a0
            csrw sscratch, zero

            # 3. 使用被调用者保存寄存器跨越 clear_bss 调用保存 SBI 参数。
            mv s0, a0
            mv s1, a1

            call {clear_bss}

            mv a0, s0
            mv a1, s1
            call kmain
        1:
            wfi
            j 1b
        ",
        clear_bss = sym clear_bss,
    )
}

extern "C" fn clear_bss(hart_id: usize) {
    if hart_id == 0 {
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
        BSS_STATE.store(BSS_READY, Ordering::Release);
    } else {
        while BSS_STATE.load(Ordering::Acquire) != BSS_READY {
            core::hint::spin_loop();
        }
    }
}
