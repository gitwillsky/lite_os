//! 只读 actual limits allocation 的 SCM capability；不依赖 Task/Process lifetime。
use super::{RLIMIT_NOFILE, ResourceLimits};

// 唯一 ResourceLimits allocation 的 scalar observer；没有 Process/PID/namespace membership。
impl crate::net::UnixFileLimit for spin::Mutex<ResourceLimits> {
    fn max_files(&self) -> usize {
        usize::try_from(self.lock().values[RLIMIT_NOFILE].soft)
            .unwrap_or(usize::MAX)
            .min(crate::file::MAX_FILE_DESCRIPTORS)
    }
}
