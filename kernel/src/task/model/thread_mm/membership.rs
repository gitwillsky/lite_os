//! Thread 的实际 MM membership；退出只保留原生授权 scalar，不保活旧 AddressSpace。
use super::AddressSpace;
use alloc::sync::Arc;

/// Live root 是唯一 mapping user；Retired 的 user_dumpable 在移交 root 时冻结。
#[derive(Clone)]
pub(in super::super) enum MmMembership {
    Live(Arc<AddressSpace>),
    Retired { user_dumpable: bool },
}
impl core::fmt::Debug for MmMembership {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 诊断只投影 membership，不进入 MM owner 或延长 root lifetime。
        formatter
            .debug_struct("MmMembership")
            .field("has_mm", &self.live().is_some())
            .finish()
    }
}
impl MmMembership {
    pub(in super::super) fn live(&self) -> Option<&Arc<AddressSpace>> {
        match self {
            Self::Live(mm) => Some(mm),
            Self::Retired { .. } => None,
        }
    }

    /// 在 outer handle guard 下冻结原生退出授权事实，并按值移交唯一 root user。
    pub(in super::super) fn retire(&mut self) -> Arc<AddressSpace> {
        let user_dumpable = self.live().expect("Thread mm retired twice").dumpable();
        match core::mem::replace(self, Self::Retired { user_dumpable }) {
            Self::Live(mm) => mm,
            Self::Retired { .. } => unreachable!("Thread mm retired twice"),
        }
    }

    /// exec 在已有 live membership 原子替换 root；没有额外 mapping user 或退出缓存。
    pub(in super::super) fn replace_live(
        &mut self,
        replacement: Arc<AddressSpace>,
    ) -> Arc<AddressSpace> {
        assert!(self.live().is_some(), "exec requires live mm");
        match core::mem::replace(self, Self::Live(replacement)) {
            Self::Live(mm) => mm,
            Self::Retired { .. } => unreachable!("exec requires live mm"),
        }
    }
}
