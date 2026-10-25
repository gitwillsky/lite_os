//! 本地时区与系统时钟：读取走 libc（`localtime_r`，因此 `/etc/localtime` 的 TZif 文件对所有程序生效），
//! 写入时区就是原子改写同一个文件，写入时钟是标准的 `clock_settime(CLOCK_REALTIME)`。
//!
//! 所有需要“本地时间”的界面（任务栏时钟、日期/时间属性、文件时间戳）都经过 [`to_local`]，不各自假设 UTC。

use std::{
    fs, io,
    sync::atomic::{AtomicBool, Ordering},
};

const LOCALTIME_PATH: &str = "/etc/localtime";

/// `unix`（自 epoch 起的 UTC 秒）处的本地时区偏移（秒，东为正）；读取失败按 UTC。
pub fn local_offset(unix: i64) -> i64 {
    linux_uapi::time::utc_offset(unix).unwrap_or(0)
}

/// UTC 秒换成“本地墙钟秒”（可直接交给 [`crate::civil_from_days`] 得到本地日期）。
pub fn to_local(unix: i64) -> i64 {
    unix + local_offset(unix)
}

/// 本地墙钟秒换回 UTC 秒；固定偏移时区下精确，有夏令时的时区在重叠小时取先出现的那次。
pub fn from_local(local: i64) -> i64 {
    local - local_offset(local - local_offset(local))
}

/// `unix` 处的时区缩写（如 `CST`）。
pub fn abbreviation(unix: i64) -> String {
    linux_uapi::time::local_time(unix).map_or_else(|_| String::new(), |time| time.abbreviation)
}

/// 让本进程重新读取 `/etc/localtime`。
///
/// musl 的 `tzset` 只在 `TZ` 字符串变化时重读文件；交替设置两个等价写法（`:` 前缀表示“这是文件路径”）
/// 就能强制重读，长驻进程（桌面外壳）在时区被改写后据此刷新。
pub fn reload() {
    // OWNER: 本模块单线程 UI 重载拥有 TZ 的等价路径切换；缺少切换 musl 会复用旧文件内容。
    static ALTERNATE: AtomicBool = AtomicBool::new(false);
    let value = if ALTERNATE.fetch_xor(true, Ordering::Relaxed) {
        format!(":{LOCALTIME_PATH}")
    } else {
        LOCALTIME_PATH.to_owned()
    };
    // SAFETY: 唯一调用者为 desktop/control-panel 的单线程主循环，无并发环境访问。
    // 后续引入进程内后台线程时必须先替换这条进程环境重载策略。
    unsafe { std::env::set_var("TZ", value) };
    linux_uapi::time::refresh_timezone();
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
    linux_uapi::time::set_realtime(unix)
}
