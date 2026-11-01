//! native 调用的寄存器约定（见 `native_abi` 顶层文档）。

use native_abi::Status;

/// 发起编号为 `number` 的 native 调用。
///
/// # Safety
///
/// 参数中的指针必须满足该调用文档规定的有效性；调用可能写入这些指针指向的内存。
#[inline(always)]
pub unsafe fn call(number: usize, arguments: [usize; 7]) -> Status {
    let status: usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: svc 只按 native ABI 读取 x0..x6/x8 并写回 x0；内存效果由调用者保证。
    unsafe {
        core::arch::asm!(
            "svc #0",
            inlateout("x0") arguments[0] => status,
            in("x1") arguments[1],
            in("x2") arguments[2],
            in("x3") arguments[3],
            in("x4") arguments[4],
            in("x5") arguments[5],
            in("x6") arguments[6],
            in("x8") number,
            options(nostack),
        );
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: ecall 只按 native ABI 读取 a0..a7 并写回 a0；内存效果由调用者保证。
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") arguments[0] => status,
            in("a1") arguments[1],
            in("a2") arguments[2],
            in("a3") arguments[3],
            in("a4") arguments[4],
            in("a5") arguments[5],
            in("a6") arguments[6],
            in("a7") number,
            options(nostack),
        );
    }
    status as Status
}

/// 以至多 7 个参数发起调用，未给出的参数为 0。
///
/// # Safety
///
/// 同 [`call`]。
#[inline(always)]
pub unsafe fn invoke(number: usize, arguments: &[usize]) -> Status {
    let mut registers = [0; 7];
    registers[..arguments.len()].copy_from_slice(arguments);
    // SAFETY: 由调用者保证。
    unsafe { call(number, registers) }
}

/// 把状态码转成 `Result`。
///
/// # Errors
///
/// 非 [`native_abi::OK`] 的状态原样返回。
pub fn check(status: Status) -> Result<(), Status> {
    if status == native_abi::OK { Ok(()) } else { Err(status) }
}
