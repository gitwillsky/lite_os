/// 通用的 SBI 调用函数。
///
/// # Arguments
/// * `eid`: SBI Extension ID (放入 x7)
/// * `fid`: SBI Function ID (放入 x6)
/// * `args`: 一个包含最多6个参数的数组，将依次放入 x0-x5。
///           args[0] -> x0, args[1] -> x1, ..., args[5] -> x5
///
/// # Returns
/// 一个元组 `(isize, isize)`，分别对应 SBI 调用返回的 `x0` (错误码) 和 `x1` (值)。
///
/// # Safety
/// 调用者必须确保提供的 EID, FID 和参数对于目标 SBI 实现是有效的。
#[inline(always)]
pub fn sbi_call(eid: usize, fid: usize, args: [usize; 6]) -> (isize, isize) {
    let mut error_code: isize;
    let mut result_value: isize;

    unsafe {
        core::arch::asm!(
            "ecall",
            // input
            in("x17") eid,
            in("x16") fid,

            inlateout("x10") args[0] => error_code,
            inlateout("x11") args[1] => result_value,
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
        )
    }
    (result_value, error_code)
}

pub fn console_putchar(c: usize) {
    sbi_call(0x01, 0, [c, 0, 0, 0, 0, 0]);
}

pub fn shutdown() {
    // SRST (System Reset Extension) EID = 0x53525354 ("SRST")
    // FID = 0 (sbi_system_reset)
    // reset_type = 0 (shutdown), reset_reason = 0 (no reason)
    sbi_call(0x53525354, 0, [0, 0, 0, 0, 0, 0]);
}

pub fn set_timer(timer_value: usize) {
    // 0x54494D45 = ASCII "TIME"
    sbi_call(0x54494D45, 0, [timer_value, 0, 0, 0, 0, 0]);
}

pub fn console_getchar() -> isize {
    let (_, ch) = sbi_call(0x02, 0, [0; 6]);
    ch
}
