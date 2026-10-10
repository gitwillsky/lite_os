//! UTS namespace 的唯一 mutable hostname/domainname；system build facts 不随 namespace 改变。
use super::quota::{UserResource, UserResourceCharge};
use super::{NamespaceAuthority, NamespaceError, UserNamespace};
use alloc::sync::Arc;
use spin::Mutex;
use syscall_abi::capability::Capability;

/// 标准 new_utsname 两个 namespace-owned 字段；每列65字节，输入可含非 UTF-8 与内嵌 NUL。
#[derive(Clone, Copy)]
pub(crate) struct UtsNames {
    pub(crate) hostname: [u8; 65],
    pub(crate) domainname: [u8; 65],
}
impl UtsNames {
    fn field(bytes: &[u8]) -> Result<[u8; 65], NamespaceError> {
        if bytes.len() > 64 {
            return Err(NamespaceError::Invalid);
        }
        let mut field = [0; 65];
        field[..bytes.len()].copy_from_slice(bytes);
        Ok(field)
    }
}

/// Immutable nsfs identity/owning user namespace 与同锁 name tuple；clone共享，NEWUTS复制。
///
/// names guard 只拷贝固定130字节，不分配、不调用 credential/user-copy；新对象在锁外分配。
pub(crate) struct UtsNamespace {
    id: u64,
    user: Arc<UserNamespace>,
    names: Mutex<UtsNames>,
    _charge: Option<UserResourceCharge>,
}
impl core::fmt::Debug for UtsNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UtsNamespace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl UtsNamespace {
    pub(super) fn initial(
        user: Arc<UserNamespace>,
        hostname: &[u8],
        domainname: &[u8],
    ) -> Result<Arc<Self>, NamespaceError> {
        Self::allocate(
            2,
            user,
            UtsNames {
                hostname: UtsNames::field(hostname)?,
                domainname: UtsNames::field(domainname)?,
            },
            None,
        )
    }
    fn allocate(
        id: u64,
        user: Arc<UserNamespace>,
        names: UtsNames,
        charge: Option<UserResourceCharge>,
    ) -> Result<Arc<Self>, NamespaceError> {
        Arc::try_new(Self {
            id,
            user,
            names: Mutex::new(names),
            _charge: charge,
        })
        .map_err(|_| NamespaceError::OutOfMemory)
    }
    pub(crate) fn copy_for(
        &self,
        user: Arc<UserNamespace>,
        creator_uid: u32,
    ) -> Result<Arc<Self>, NamespaceError> {
        let charge = UserResourceCharge::acquire(&user, creator_uid, UserResource::Uts)?;
        let names = *self.names.lock();
        Self::allocate(super::next_namespace_id(), user, names, Some(charge))
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
    pub(crate) fn snapshot(&self) -> UtsNames {
        *self.names.lock()
    }
    /// 原子更新一列；先校验 namespace capability，成功前完整准备标准 NUL/padding tuple。
    pub(crate) fn set_name(
        &self,
        hostname: bool,
        bytes: &[u8],
        caller: &NamespaceAuthority,
    ) -> Result<(), NamespaceError> {
        if !caller.capable(&self.user, Capability::SYS_ADMIN) {
            return Err(NamespaceError::Permission);
        }
        let prepared = UtsNames::field(bytes)?;
        let mut names = self.names.lock();
        if hostname {
            names.hostname = prepared;
        } else {
            names.domainname = prepared;
        }
        Ok(())
    }
}
