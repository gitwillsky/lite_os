use core::{
    arch::naked_asm,
    sync::atomic::{AtomicUsize, Ordering},
};

const BSS_PENDING: usize = 0x4253_5350;
const BSS_CLEARING: usize = 0x4253_5343;
const BSS_READY: usize = 0x4253_5352;

// 该标志使用非零初值以确保位于 .data；若放在尚未清零的 BSS，次核可能误判初始化已完成。
static BSS_STATE: AtomicUsize = AtomicUsize::new(BSS_PENDING);

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
            # 1. 固定栈数组只能由合法 hart ID 索引；越界 hart 在接触任何栈前 fail-stop。
            li t0, {max_cores}
            bgeu a0, t0, 2f

            # 2. 每个 hart 使用 linker 预留的独立 128 KiB 启动栈。
            slli t0, a0, 17
            la sp, boot_stack_top
            sub sp, sp, t0

            # 3. 初始化内核 psABI 固定寄存器；用户 gp/tp 将由 trampoline 单独保存。
            .option push
            .option norelax
            la gp, __global_pointer$
            .option pop
            mv tp, a0
            csrw sscratch, zero

            # 4. 使用被调用者保存寄存器跨越 clear_bss 调用保存 SBI 参数。
            mv s0, a0
            mv s1, a1

            call {clear_bss}
            mv s2, a0

            mv a0, s0
            mv a1, s1
            mv a2, s2
            call kmain
        1:
            wfi
            j 1b
        2:
            csrci sstatus, 2
            csrw sie, zero
        3:
            wfi
            j 3b
        ",
        max_cores = const crate::arch::hart::MAX_SUPPORTED_HARTS,
        clear_bss = sym clear_bss,
    )
}

/// @description 由唯一胜出的 hart 清零 BSS，并向其他 hart 发布完成状态。
///
/// @param _hart_id SBI 传入的当前 hart ID；入口已完成边界验证。
/// @return `1` 表示当前 hart 是全局初始化所有者，`0` 表示当前 hart 是 secondary。
extern "C" fn clear_bss(_hart_id: usize) -> usize {
    if BSS_STATE
        .compare_exchange(
            BSS_PENDING,
            BSS_CLEARING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
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
        // Release 发布所有 BSS 清零写；缺失时 secondary 可能把未清零的 Once/锁当作已初始化。
        BSS_STATE.store(BSS_READY, Ordering::Release);
        1
    } else {
        // Acquire 消费清零写，之后才允许 Rust 代码访问 BSS 中的全局对象。
        while BSS_STATE.load(Ordering::Acquire) != BSS_READY {
            core::hint::spin_loop();
        }
        0
    }
}
