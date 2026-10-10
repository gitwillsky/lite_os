//! Credential/files fixture 的显式 mount topology seam；实际 mount copy/roots 在 fs_root_lookup 执行。
use crate::namespace::{NamespaceError, NamespaceOwner, NamespaceType, UserNamespace};
use alloc::sync::Arc;

#[derive(Debug)]
pub(crate) struct MountNamespace(Arc<UserNamespace>);
pub(crate) fn initial() -> Arc<MountNamespace> {
    static INITIAL: std::sync::OnceLock<Arc<MountNamespace>> = std::sync::OnceLock::new();
    INITIAL
        .get_or_init(|| Arc::new(MountNamespace(crate::namespace::boot())))
        .clone()
}
impl MountNamespace {
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.0.clone()
    }
    pub(crate) fn copy_for(
        &self,
        _: Arc<UserNamespace>,
        _: u32,
        _: crate::fs::LookupOrigin,
    ) -> Result<(Arc<Self>, crate::fs::LookupOrigin), NamespaceError> {
        unreachable!("mount topology runs in fs_root_lookup")
    }
}
impl NamespaceOwner for MountNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::Mount
    }
    fn id(&self) -> u64 {
        1
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.user_namespace())
    }
}
