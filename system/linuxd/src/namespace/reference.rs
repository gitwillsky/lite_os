//! nsfs/proc 暴露的具体 namespace owner 引用；显示名字不能重新解析为 membership。
use super::{PidNamespace, TimeNamespace, UserNamespace, UtsNamespace};
use alloc::sync::Arc;

/// 固定 Linux namespace clone flag，既用于 setns 类型校验也用于 NS_GET_NSTYPE。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NamespaceType {
    User,
    Uts,
    Mount,
    Time,
    Pid,
    Ipc,
    Net,
}
impl NamespaceType {
    pub(crate) fn flag(self) -> u32 {
        match self {
            Self::User => 0x1000_0000,
            Self::Uts => 0x0400_0000,
            Self::Mount => 0x0002_0000,
            Self::Time => 0x80,
            Self::Pid => 0x2000_0000,
            Self::Ipc => 0x0800_0000,
            Self::Net => 0x4000_0000,
        }
    }
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::User => b"user",
            Self::Uts => b"uts",
            Self::Mount => b"mnt",
            Self::Time => b"time",
            Self::Pid => b"pid",
            Self::Ipc => b"ipc",
            Self::Net => b"net",
        }
    }
}

/// proc membership selector；for_children 打开同一实际 nsfs 类型，不新建虚假 namespace 类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NamespaceSelector {
    Namespace(NamespaceType),
    TimeForChildren,
    PidForChildren,
}
impl NamespaceSelector {
    pub(crate) const ALL: [Self; 9] = [
        Self::Namespace(NamespaceType::User),
        Self::Namespace(NamespaceType::Uts),
        Self::Namespace(NamespaceType::Mount),
        Self::Namespace(NamespaceType::Time),
        Self::TimeForChildren,
        Self::Namespace(NamespaceType::Pid),
        Self::PidForChildren,
        Self::Namespace(NamespaceType::Ipc),
        Self::Namespace(NamespaceType::Net),
    ];
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::Namespace(kind) => kind.name(),
            Self::TimeForChildren => b"time_for_children",
            Self::PidForChildren => b"pid_for_children",
        }
    }
}

/// 实际 namespace 的只读 identity seam；具体内容与 clone/setns mutation 留在各领域 owner。
/// 方法只读取 immutable identity/ancestry，不取得 topology、Task 或 syscall/user-copy 锁。
/// Any 仅供 membership owner 做同一 Arc 的显式类型检查；不分配、不复制内容或产生备用 owner。
pub(crate) trait NamespaceOwner: core::any::Any + core::fmt::Debug + Send + Sync {
    fn kind(&self) -> NamespaceType;
    fn id(&self) -> u64;
    fn owning_user(&self) -> Option<Arc<UserNamespace>>;
    /// 仅具有 parent namespace 语义的类型实现；无此 ioctl 返回 Invalid/EINVAL。
    fn parent_reference(&self) -> Result<Option<NamespaceReference>, super::NamespaceError> {
        Err(super::NamespaceError::Invalid)
    }
}

/// 只保活实际 namespace；不保活产生 proc 链接的 Thread/MM/files，不缓存 identity 副本。
#[derive(Clone, Debug)]
pub(crate) struct NamespaceReference(Arc<dyn NamespaceOwner>);
impl NamespaceReference {
    pub(crate) fn new<T: NamespaceOwner>(owner: Arc<T>) -> Self {
        Self(owner)
    }
    pub(crate) fn kind(&self) -> NamespaceType {
        self.0.kind()
    }
    pub(crate) fn id(&self) -> u64 {
        self.0.id()
    }
    pub(crate) fn inode(&self) -> u64 {
        match (self.kind(), self.id()) {
            (NamespaceType::User, 3) => 0xefff_fffd,
            (NamespaceType::Uts, 2) => 0xefff_fffe,
            (NamespaceType::Mount, 1) => 0xf000_0000,
            (NamespaceType::Time, 8) => 0xefff_fffa,
            (NamespaceType::Pid, 4) => 0xefff_fffc,
            (NamespaceType::Ipc, 5) => 0xefff_ffff,
            (NamespaceType::Net, 6) => 0xefff_fffb,
            _ => self.id(),
        }
    }
    pub(crate) fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        self.0.owning_user()
    }
    pub(crate) fn parent_reference(&self) -> Result<Option<Self>, super::NamespaceError> {
        self.0.parent_reference()
    }
    /// 显式 typed membership validation；沿同一 owner Arc 转换，不分配，失配返回 Invalid。
    pub(crate) fn downcast<T: NamespaceOwner>(&self) -> Result<Arc<T>, super::NamespaceError> {
        let owner: Arc<dyn core::any::Any + Send + Sync> = self.0.clone();
        owner.downcast().map_err(|_| super::NamespaceError::Invalid)
    }
}
impl NamespaceOwner for UserNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::User
    }
    fn id(&self) -> u64 {
        self.id()
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        self.parent()
    }
    fn parent_reference(&self) -> Result<Option<NamespaceReference>, super::NamespaceError> {
        Ok(self.parent().map(NamespaceReference::new))
    }
}
impl NamespaceOwner for UtsNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::Uts
    }
    fn id(&self) -> u64 {
        self.id()
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.user_namespace())
    }
}
impl NamespaceOwner for TimeNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::Time
    }
    fn id(&self) -> u64 {
        self.id()
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.user_namespace())
    }
}

impl NamespaceOwner for PidNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::Pid
    }
    fn id(&self) -> u64 {
        self.id()
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.user_namespace())
    }
    fn parent_reference(&self) -> Result<Option<NamespaceReference>, super::NamespaceError> {
        // parent visibility 按 calling active PID namespace 在 nsfs ioctl seam 检查。
        Ok(self.parent().map(NamespaceReference::new))
    }
}
