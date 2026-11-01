//! 内核启动参数（DTB `/chosen/bootargs`）中内核自身消费的选项。
//!
//! 只识别 `loglevel=N`、`quiet`、`debug` 与 `panic=N`；其余选项原样交给根服务。

/// 内核消费的启动参数。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct KernelParameters {
    /// 日志输出阈值（`loglevel=`，`quiet` 为 4，`debug` 为 10）。
    pub(crate) log_level: Option<u8>,
    /// `panic=`：0 停在蓝屏，正数为秒数后重启，负数立即重启。
    pub(crate) panic_seconds: Option<i32>,
}

/// 选项值无法解析的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandLineError {
    /// `loglevel=` 的值不是十进制整数。
    InvalidLogLevel,
    /// `panic=` 的值不是十进制整数。
    InvalidPanicTimeout,
}

fn decimal(value: &[u8]) -> Option<i64> {
    let (negative, digits) = match value.strip_prefix(b"-") {
        Some(digits) => (true, digits),
        None => (false, value),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut number: i64 = 0;
    for digit in digits {
        number = number.checked_mul(10)?.checked_add(i64::from(digit - b'0'))?;
    }
    Some(if negative { -number } else { number })
}

/// 按空白分词并取出内核选项。
///
/// # Errors
/// 选项值非法时返回 [`CommandLineError`]。
pub(crate) fn parse(line: &[u8]) -> Result<KernelParameters, CommandLineError> {
    let mut parameters = KernelParameters::default();
    for word in line.split(u8::is_ascii_whitespace).filter(|word| !word.is_empty()) {
        if word == b"quiet" {
            parameters.log_level = Some(4);
        } else if word == b"debug" {
            parameters.log_level = Some(10);
        } else if let Some(value) = word.strip_prefix(b"loglevel=") {
            let level = decimal(value)
                .and_then(|level| u8::try_from(level).ok())
                .ok_or(CommandLineError::InvalidLogLevel)?;
            parameters.log_level = Some(level);
        } else if let Some(value) = word.strip_prefix(b"panic=") {
            let seconds = decimal(value)
                .and_then(|seconds| i32::try_from(seconds).ok())
                .ok_or(CommandLineError::InvalidPanicTimeout)?;
            parameters.panic_seconds = Some(seconds);
        }
    }
    Ok(parameters)
}
