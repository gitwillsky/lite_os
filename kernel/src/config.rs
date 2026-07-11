use crate::log::LogLevel;

// 时钟中断的频率
pub(crate) const TICKS_PER_SEC: usize = 100;

// 日志配置
#[cfg(debug_assertions)]
pub(crate) const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Debug;

#[cfg(not(debug_assertions))]
pub(crate) const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;
