//! Caller limits 的 scalar input seam；实际 max_files getter/Mutex/clamp 直接加载生产 leaf。
use alloc::sync::Arc;
const RLIMIT_NOFILE: usize = 7;
#[derive(Clone, Copy)]
struct ResourceLimit {
    soft: u64,
    hard: u64,
}
struct ResourceLimits {
    values: [ResourceLimit; 16],
    last_cpu_signal_second: Option<u64>,
}
#[path = "../../../../kernel/src/execution/task/model/resource_limits/unix_file_limit.rs"]
mod limits_owner;
/// Host producer 只设置当前 limits 输入，不实现检查、clamp、quota 或恢复算法。
pub(crate) struct ProducedLimit {
    owner: Arc<spin::Mutex<ResourceLimits>>,
}
impl ProducedLimit {
    pub(crate) fn new(value: usize) -> Arc<Self> {
        let mut values = [ResourceLimit {
            soft: u64::MAX,
            hard: u64::MAX,
        }; 16];
        values[RLIMIT_NOFILE].soft = value as u64;
        Arc::new(Self {
            owner: Arc::new(spin::Mutex::new(ResourceLimits {
                values,
                last_cpu_signal_second: None,
            })),
        })
    }
    pub(crate) fn set(&self, value: usize) {
        self.owner.lock().values[RLIMIT_NOFILE].soft = value as u64;
    }
}
impl crate::net::UnixFileLimit for ProducedLimit {
    fn max_files(&self) -> usize {
        crate::net::UnixFileLimit::max_files(self.owner.as_ref())
    }
}
