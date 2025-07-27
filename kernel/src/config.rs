use crate::log::LogLevel;

// 日志配置
#[cfg(debug_assertions)]
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Debug;

#[cfg(not(debug_assertions))]
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;
