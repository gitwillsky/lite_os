use core::arch::naked_asm;

use super::hart;

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
// SAFETY: firmware enters with SBI a0/a1, paging disabled, and no Rust stack; this naked entry
// establishes gp/tp/sscratch and a topology-owned stack before calling any Rust function.
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
            # 1. 未发布动态表表示唯一 cold-boot hart；它使用 linker early stack。
            la   t0, {table_address}
            ld   t0, 0(t0)
            li   t1, -1
            beq  t0, t1, 3f

            # 2. secondary 消费 boot hart 发布的动态表，按 DTB hart ID 查找启动栈。
            fence r, rw
            la   t1, {table_length}
            ld   t1, 0(t1)
            mv   t2, t0
        1:
            beqz t1, 5f
            ld   t3, {id_offset}(t2)
            beq  t3, a0, 2f
            li   t4, {state_size}
            add  t2, t2, t4
            addi t1, t1, -1
            j    1b
        2:
            ld   sp, {stack_top_offset}(t2)
            j    4f
        3:
            la   sp, boot_stack_top

            .option push
            .option norelax
            la   gp, __global_pointer$
            .option pop
            mv   tp, a0
            csrw sscratch, zero
            mv   s0, a0
            mv   s1, a1
            call {clear_bss}
            mv   a0, s0
            mv   a1, s1
            call {boot_main}
            j    5f

        4:
            .option push
            .option norelax
            la   gp, __global_pointer$
            .option pop
            mv   tp, a0
            csrw sscratch, zero
            call {secondary_main}

        5:
            csrci sstatus, 2
            csrw sie, zero
        6:
            wfi
            j 6b
        ",
        table_address = sym hart::HART_TABLE_ADDRESS,
        table_length = sym hart::HART_TABLE_LENGTH,
        id_offset = const hart::HART_STATE_ID_OFFSET,
        stack_top_offset = const hart::HART_STATE_STACK_TOP_OFFSET,
        state_size = const hart::HART_STATE_SIZE,
        clear_bss = sym clear_bss,
        boot_main = sym crate::kmain_boot,
        secondary_main = sym crate::kmain_secondary,
    )
}

/// @description 获取 SBI HSM secondary 使用的统一 S-mode 入口。
///
/// @return `_start` 的物理入口地址。
/// @errors 无错误。
pub(crate) fn entry_address() -> usize {
    _start as *const () as usize
}

/// @description 由唯一 cold-boot hart 清零 BSS。
///
/// @errors bootloader 若错误地同时放行多个 hart，会破坏该单写者前提。
extern "C" fn clear_bss() {
    // SAFETY: linker script provides immutable address symbols delimiting the kernel BSS.
    unsafe extern "C" {
        fn sbss();
        fn ebss();
    }
    // SAFETY: linker symbols delimit the writable BSS range; the unique cold-boot hart executes
    // this before publishing any reference into BSS, so byte-wise zeroing has no aliases.
    unsafe {
        // Linker boundaries are declared consistently as address-only function symbols across
        // arch and memory so fat LTO cannot split the same ELF symbol into conflicting LLVM types.
        let start = sbss as *const () as usize;
        let end = ebss as *const () as usize;
        let count = end - start;
        if count > 0 {
            core::ptr::write_bytes(start as *mut u8, 0, count);
        }
    }
}
