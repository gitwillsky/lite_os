use crate::aclint::SifiveClint as Clint;
use crate::clint::CLINT;
use crate::fast_trap::trap_entry;
use crate::rfence::{ACKNOWLEDGED, REQUEST_FENCE_I, REQUEST_SFENCE_VMA, REQUESTS};
use core::arch::naked_asm;

/// 中断向量表
///
/// # Safety
///
/// 裸函数。
#[unsafe(naked)]
// SAFETY: mtvec uses this aligned vectored table only in M-mode; every slot transfers directly to
// a naked handler that preserves the interrupted supervisor context.
pub(crate) unsafe extern "C" fn trap_vec() {
    naked_asm!(
        ".align 2",
        ".option push",
        ".option norvc",
        "j {default}", // exception
        "j {default}", // supervisor software
        "j {default}", // reserved
        "j {msoft} ",  // machine    software
        "j {default}", // reserved
        "j {default}", // supervisor timer
        "j {default}", // reserved
        "j {mtimer}",  // machine    timer
        "j {default}", // reserved
        "j {default}", // supervisor external
        "j {default}", // reserved
        "j {default}", // machine    external
        ".option pop",
        default = sym trap_entry,
        mtimer  = sym mtimer,
        msoft   = sym msoft,
    )
}

/// machine timer 中断代理
///
/// # Safety
///
/// 裸函数。
#[unsafe(naked)]
// SAFETY: entered only from trap_vec for MTIP with a valid mscratch stack; the routine saves every
// clobbered ABI register, bounds CLINT access through the initialized hart mapping, then mret.
unsafe extern "C" fn mtimer() {
    naked_asm!(
        // 换栈：
        // sp      : M sp
        // mscratch: S sp
        "   csrrw sp, mscratch, sp",
        // 保护
        "   addi  sp, sp, -4*8
            sd    ra, 0*8(sp)
            sd    a0, 1*8(sp)
            sd    a1, 2*8(sp)
            sd    a2, 3*8(sp)
        ",
        // 清除 mtimecmp
        "   la    a0, {clint_ptr}
            ld    a0, (a0)
            csrr  a1, mhartid
            addi  a2, zero, -1
            call  {set_mtimecmp}
        ",
        // 设置 stip
        "   li    a0, {mip_stip}
            csrrs zero, mip, a0
        ",
        // 恢复
        "   ld    ra, 0*8(sp)
            ld    a0, 1*8(sp)
            ld    a1, 2*8(sp)
            ld    a2, 3*8(sp)
            addi  sp, sp,  4*8
        ",
        // 换栈：
        // sp      : S sp
        // mscratch: M sp
        "   csrrw sp, mscratch, sp",
        // 返回
        "   mret",
        mip_stip     = const 1 << 5,
        clint_ptr    =   sym CLINT,
        //                   Clint::write_mtimecmp_naked(&self, hart_idx, val)
        set_mtimecmp =   sym Clint::write_mtimecmp_naked,
    )
}

/// machine soft 中断代理
///
/// # Safety
///
/// 裸函数。
#[unsafe(naked)]
// SAFETY: entered only from trap_vec for MSIP with a valid mscratch stack; all clobbered registers
// are saved and restored around RFENCE/HSM handling before mret.
unsafe extern "C" fn msoft() {
    naked_asm!(
        ".option arch, +a",
        // 换栈：
        // sp      : M sp
        // mscratch: S sp
        "   csrrw sp, mscratch, sp",
        // 保护
        "   addi sp, sp, -4*8
            sd   ra, 0*8(sp)
            sd   a0, 1*8(sp)
            sd   a1, 2*8(sp)
            sd   a2, 3*8(sp)
        ",
        // 清除 msip
        "   la   a0, {clint_ptr}
            ld   a0, (a0)
            csrr a1, mhartid
            call {clear_msip}
        ",
        // 1. aq swap 消费 RFENCE sender 的 Release request。
        "   la   a0, {rfence_requests}
            csrr a1, mhartid
            slli a1, a1, 3
            add  a0, a0, a1
            amoswap.d.aq a2, zero, (a0)
            beqz a2, 3f

            andi a0, a2, {request_fence_i}
            beqz a0, 1f
            fence.i
         1:
            andi a0, a2, {request_sfence_vma}
            beqz a0, 2f
            sfence.vma
         2:
            # 2. rl ack 发布 fence 完成；sender Acquire 看到 ack 后才从 SBI 返回。
            la   a0, {rfence_acknowledged}
            add  a0, a0, a1
            li   a2, 1
            amoswap.d.rl zero, a2, (a0)
         3:
            # 普通 IPI 与 RFENCE 可以合并在同一个 MSIP 中，始终向 S-mode 转发 SSIP。
            csrrsi zero, mip, 1 << 1
        ",
        // 恢复
        "   ld   ra, 0*8(sp)
            ld   a0, 1*8(sp)
            ld   a1, 2*8(sp)
            ld   a2, 3*8(sp)
            addi sp, sp,  4*8
        ",
        // 换栈：
        // sp      : S sp
        // mscratch: M sp
        "   csrrw sp, mscratch, sp",
        // 返回
        "   mret",
        clint_ptr  = sym CLINT,
        //               Clint::clear_msip_naked(&self, hart_idx)
        clear_msip = sym Clint::clear_msip_naked,
        rfence_requests = sym REQUESTS,
        rfence_acknowledged = sym ACKNOWLEDGED,
        request_fence_i = const REQUEST_FENCE_I,
        request_sfence_vma = const REQUEST_SFENCE_VMA,
    )
}
