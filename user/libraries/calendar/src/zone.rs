//! 按标准 TZif 文件解析本地时区；不修改进程 TZ 环境，支持 UI 与后台线程同时查询/重载。
use std::{
    fs,
    io::{self, Write},
    sync::{OnceLock, RwLock},
};
use tz::TimeZone;
const LOCALTIME_PATH: &str = "/etc/localtime";
// OWNER: calendar 独占进程内已解析时区；锁内只查询或替换不可变 TZif 模型，不执行文件 I/O。
// PROOF: 重载先在锁外完整解析，再一次替换；读者不会观察半份转换表，也不借用 libc 全局指针。
// FAILURE: 若在后台线程存在时切换环境变量，会违反 std::env::set_var 的线程安全前置条件。
static TIMEZONE: OnceLock<RwLock<TimeZone>> = OnceLock::new();
fn timezone() -> &'static RwLock<TimeZone> {
    TIMEZONE.get_or_init(|| RwLock::new(read_timezone().unwrap_or_else(|_| TimeZone::utc())))
}
fn read_timezone() -> io::Result<TimeZone> {
    let bytes = fs::read(LOCALTIME_PATH)?;
    TimeZone::from_tz_data(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}
/// 查询 UTC 秒处东向偏移；未安装时区或超出解析范围时按 UTC。
pub fn local_offset(unix: i64) -> i64 {
    timezone()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .find_local_time_type(unix)
        .map_or(0, |kind| i64::from(kind.ut_offset()))
}
/// UTC 秒换成本地墙钟秒。
pub fn to_local(unix: i64) -> i64 {
    unix.saturating_add(local_offset(unix))
}
/// 本地墙钟秒换回 UTC；重叠小时选择较早的时刻，不存在的墙钟由 [`from_local_checked`] 报错。
pub fn from_local(local: i64) -> i64 {
    from_local_checked(local).unwrap_or_else(|_| local.saturating_sub(local_offset(local)))
}
/// 精确解析本地墙钟；无法表示或处于夏令时跳过区间时返回 InvalidInput。
pub fn from_local_checked(local: i64) -> io::Result<i64> {
    let date = crate::civil_from_days(local.div_euclid(86400));
    let year = i32::try_from(date.year)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "year out of range"))?;
    let seconds = local.rem_euclid(86400);
    let zone = timezone().read().unwrap_or_else(|error| error.into_inner());
    let mut values = [None; 4];
    let found = tz::DateTime::find_n(
        &mut values,
        year,
        date.month as u8,
        date.day as u8,
        (seconds / 3600) as u8,
        (seconds % 3600 / 60) as u8,
        (seconds % 60) as u8,
        0,
        zone.as_ref(),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    if !found.is_exhaustive() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ambiguous timezone exceeds supported candidates",
        ));
    }
    found
        .data()
        .iter()
        .flatten()
        .filter_map(|kind| match kind {
            tz::datetime::FoundDateTimeKind::Normal(value) => Some(value.unix_time()),
            _ => None,
        })
        .min()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "local time does not exist"))
}
/// 查询 UTC 秒处的时区缩写；超出范围返回空字符串。
pub fn abbreviation(unix: i64) -> String {
    timezone()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .find_local_time_type(unix)
        .map_or_else(
            |_| String::new(),
            |kind| kind.time_zone_designation().to_owned(),
        )
}
/// 重新读取 `/etc/localtime`；损坏或读取错误保留原模型，返回具体错误，不修改环境变量。
pub fn reload() -> io::Result<()> {
    let replacement = read_timezone()?;
    *timezone()
        .write()
        .unwrap_or_else(|error| error.into_inner()) = replacement;
    Ok(())
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
    let replacement = TimeZone::from_tz_data(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let mut temporary = None;
    for index in 0..128 {
        let path = format!("{LOCALTIME_PATH}.{}.{}.new", std::process::id(), index);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                temporary = Some((path, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (path, mut file) = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "timezone temporary files exhausted",
        )
    })?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&path, LOCALTIME_PATH)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result?;
    *timezone()
        .write()
        .unwrap_or_else(|error| error.into_inner()) = replacement;
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
