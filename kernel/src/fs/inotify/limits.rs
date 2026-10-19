//! 标准 inotify sysctl：global queue policy 与 initial UserNS 的同一 ucounts limits。
use crate::namespace::UserResource;
use core::sync::atomic::{AtomicU32, Ordering};

/// 标准 fs/inotify sysctl；两个 user 上限与 /proc/sys/user 引用同一 initial ucounts owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum InotifyLimit {
    Instances,
    Watches,
    QueuedEvents,
}
// OWNER: 全局 sysctl 的未来 instance 队列上限；实际打开的队列独占创建时快照。
pub(super) static QUEUED_EVENT_LIMIT: AtomicU32 = AtomicU32::new(16384);
impl InotifyLimit {
    pub(crate) const ALL: [Self; 3] = [Self::Instances, Self::Watches, Self::QueuedEvents];
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::Instances => b"max_user_instances",
            Self::Watches => b"max_user_watches",
            Self::QueuedEvents => b"max_queued_events",
        }
    }
    pub(crate) fn value(self) -> u32 {
        match self {
            Self::Instances => {
                crate::namespace::initial_user().resource_limit(UserResource::InotifyInstances)
            }
            Self::Watches => {
                crate::namespace::initial_user().resource_limit(UserResource::InotifyWatches)
            }
            Self::QueuedEvents => QUEUED_EVENT_LIMIT.load(Ordering::Relaxed),
        }
    }
    /// 固定全局0644 EUID policy；范围0..INT_MAX，缺失时容量会被转成无界 usize。
    pub(crate) fn set(
        self,
        value: u32,
        caller: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), crate::namespace::NamespaceError> {
        use crate::namespace::NamespaceError;
        if caller.effective_uid != 0 {
            return Err(NamespaceError::Permission);
        }
        if value > i32::MAX as u32 {
            return Err(NamespaceError::Invalid);
        }
        match self {
            Self::QueuedEvents => {
                QUEUED_EVENT_LIMIT.store(value, Ordering::Relaxed);
                Ok(())
            }
            Self::Instances => crate::namespace::set_initial_inotify_limit(
                UserResource::InotifyInstances,
                value,
                caller,
            ),
            Self::Watches => crate::namespace::set_initial_inotify_limit(
                UserResource::InotifyWatches,
                value,
                caller,
            ),
        }
    }
}
