//! 本地时区与系统时钟：读取走 libc（`localtime_r`，因此 `/etc/localtime` 的 TZif 文件对所有程序生效），
//! 写入时区就是原子改写同一个文件，写入时钟是标准的 `clock_settime(CLOCK_REALTIME)`。
//!
//! 所有需要“本地时间”的界面（任务栏时钟、日期/时间属性、文件时间戳）都经过 [`to_local`]，不各自假设 UTC。

use std::{
    ffi::{CString, c_char, c_int},
    fs, io,
    sync::atomic::{AtomicBool, Ordering},
};

/// musl 的 `struct tm`（aarch64 与 riscv64 布局相同）。
#[repr(C)]
struct Tm {
    sec: c_int,
    min: c_int,
    hour: c_int,
    mday: c_int,
    month: c_int,
    year: c_int,
    weekday: c_int,
    yearday: c_int,
    dst: c_int,
    gmtoff: i64,
    zone: *const c_char,
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanoseconds: i64,
}

unsafe extern "C" {
    fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    fn tzset();
    fn clock_settime(clock: c_int, time: *const Timespec) -> c_int;
}

const CLOCK_REALTIME: c_int = 0;
const LOCALTIME_PATH: &str = "/etc/localtime";

fn broken_down(unix: i64) -> Option<Tm> {
    let mut result = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: `unix` 与 `result` 都是有效且对齐的；localtime_r 成功时完整填充 `result`，失败返回空指针。
    let filled = unsafe { localtime_r(&unix, result.as_mut_ptr()) };
    // SAFETY: 非空返回表示 `result` 已初始化。
    (!filled.is_null()).then(|| unsafe { result.assume_init() })
}

/// `unix`（自 epoch 起的 UTC 秒）处的本地时区偏移（秒，东为正）；读取失败按 UTC。
pub fn local_offset(unix: i64) -> i64 {
    broken_down(unix).map_or(0, |tm| tm.gmtoff)
}

/// UTC 秒换成“本地墙钟秒”（可直接交给 [`crate::civil_from_days`] 与取模得到本地日期时间）。
pub fn to_local(unix: i64) -> i64 {
    unix + local_offset(unix)
}

/// 本地墙钟秒换回 UTC 秒；固定偏移时区下精确，有夏令时的时区在重叠小时取先出现的那次。
pub fn from_local(local: i64) -> i64 {
    local - local_offset(local - local_offset(local))
}

/// `unix` 处的时区缩写（如 `CST`）。
pub fn abbreviation(unix: i64) -> String {
    broken_down(unix)
        .filter(|tm| !tm.zone.is_null())
        // SAFETY: musl 的 tm_zone 指向静态时区名字符串，以 NUL 结尾。
        .map(|tm| unsafe { std::ffi::CStr::from_ptr(tm.zone) }.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 让本进程重新读取 `/etc/localtime`。
///
/// musl 的 `tzset` 只在 `TZ` 字符串变化时重读文件；交替设置两个等价写法（`:` 前缀表示“这是文件路径”）
/// 就能强制重读，长驻进程（桌面外壳）在时区被改写后据此刷新。
pub fn reload() {
    static ALTERNATE: AtomicBool = AtomicBool::new(false);
    let value = if ALTERNATE.fetch_xor(true, Ordering::Relaxed) {
        format!(":{LOCALTIME_PATH}")
    } else {
        LOCALTIME_PATH.to_owned()
    };
    if let (Ok(name), Ok(value)) = (CString::new("TZ"), CString::new(value)) {
        // SAFETY: 两个参数都是有效的 NUL 结尾字符串；只改本进程环境，调用者是单线程的界面主循环。
        unsafe {
            setenv(name.as_ptr(), value.as_ptr(), 1);
            tzset();
        }
    }
}

/// 把 `/etc/localtime` 原子改写成固定偏移（无夏令时）的 TZif v1 文件。
///
/// # Parameters
///
/// - `offset`: 东为正的偏移秒数。
/// - `abbreviation`: 时区缩写（字母、数字、`+`、`-`）。
///
/// # Errors
///
/// 没有权限或文件系统错误。
pub fn write_fixed(offset: i32, abbreviation: &str) -> io::Result<()> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"TZif\0");
    bytes.extend_from_slice(&[0; 15]);
    let chars = abbreviation.len() as u32 + 1;
    // 计数顺序：isutc、isstd、leap、transitions、types、abbreviation 字符数。
    for count in [0u32, 0, 0, 0, 1, chars] {
        bytes.extend_from_slice(&count.to_be_bytes());
    }
    bytes.extend_from_slice(&offset.to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(abbreviation.as_bytes());
    bytes.push(0);
    let temporary = format!("{LOCALTIME_PATH}.new");
    fs::write(&temporary, &bytes)?;
    fs::rename(&temporary, LOCALTIME_PATH)?;
    reload();
    Ok(())
}

/// 设置系统实时钟（`clock_settime(CLOCK_REALTIME)`）。
///
/// # Errors
///
/// 没有 `CAP_SYS_TIME`（`PermissionDenied`）或时间无效。
pub fn set_clock(unix: i64) -> io::Result<()> {
    let time = Timespec {
        seconds: unix,
        nanoseconds: 0,
    };
    // SAFETY: `time` 是有效的 timespec，调用只读取它。
    if unsafe { clock_settime(CLOCK_REALTIME, &time) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
