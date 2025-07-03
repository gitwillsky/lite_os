#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbiError {
    Success,
    Failed,
    NotSupported,
    InvalidParameter,
    Denied,
    InvalidAddress,
    AlreadyAvailable,
    AlreadyStarted,
    AlreadyStopped,
    NoShmem, // SBI 2.0
    Unknown(isize),
}

impl From<isize> for SbiError {
    fn from(value: isize) -> Self {
        match value {
            0 => SbiError::Success,
            -1 => SbiError::Failed,
            -2 => SbiError::NotSupported,
            -3 => SbiError::InvalidParameter,
            -4 => SbiError::Denied,
            -5 => SbiError::InvalidAddress,
            -6 => SbiError::AlreadyAvailable,
            -7 => SbiError::AlreadyStarted,
            -8 => SbiError::AlreadyStopped,
            -9 => SbiError::NoShmem,
            _ => SbiError::Unknown(value),
        }
    }
}

pub type SbiResult<T> = Result<T, SbiError>;

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
pub fn sbi_call(eid: usize, fid: usize, args: [usize; 6]) -> SbiResult<isize> {
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

    match SbiError::from(error_code) {
        SbiError::Success => Ok(result_value),
        _ => Err(SbiError::from(error_code)),
    }
}

pub fn console_putchar(c: usize) -> SbiResult<()> {
    sbi_call(0x01, 0, [c, 0, 0, 0, 0, 0])?;
    Ok(())
}

pub fn shutdown() -> SbiResult<()> {
    sbi_call(0x08, 0, [0; 6])?;
    Ok(())
}

pub fn set_timer(timer_value: usize) -> SbiResult<()> {
    // 0x54494D45 = ASCII "TIME"
    sbi_call(0x54494D45, 0, [timer_value, 0, 0, 0, 0, 0])?;
    Ok(())
}

pub fn console_getchar() -> isize {
    match sbi_call(0x01, 1, [0; 6]) {
        Ok(ch) => ch,
        Err(_) => -1,
    }
}
