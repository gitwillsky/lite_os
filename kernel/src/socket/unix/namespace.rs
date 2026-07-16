use alloc::sync::{Arc, Weak};
use spin::{Mutex, Once};

use crate::fallible_tree::FallibleMap;

use super::{SocketError, UnixSocket};

const UNIX_PATH_MAX: usize = 108;

/// @description Linux `sockaddr_un.sun_path` 的长度保留地址值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnixAddress {
    length: u8,
    bytes: [u8; UNIX_PATH_MAX],
}

impl UnixAddress {
    /// @description 从不含 family 的原始 sun_path bytes 构造地址。
    /// @param bytes abstract 地址包含首个 NUL；pathname 地址后续由 VFS seam 规范化。
    /// @return 保留原始长度的地址值。
    /// @errors 空地址或超过 Linux 108-byte sun_path 返回 Invalid。
    pub(crate) fn new(bytes: &[u8]) -> Result<Self, SocketError> {
        if bytes.is_empty() || bytes.len() > UNIX_PATH_MAX {
            return Err(SocketError::Invalid);
        }
        let mut address = Self {
            length: bytes.len() as u8,
            bytes: [0; UNIX_PATH_MAX],
        };
        address.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(address)
    }

    /// @description 返回调用方提供的精确 sun_path bytes。
    /// @return 不含 sockaddr family 的地址 slice。
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.length)]
    }

    /// @description 区分 Linux abstract 与 pathname namespace。
    /// @return sun_path 首字节为 NUL 时为 true。
    pub(crate) fn is_abstract(&self) -> bool {
        self.bytes().first() == Some(&0)
    }
}

/// @description VFS socket inode 的稳定 identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnixPathIdentity {
    pub(crate) filesystem: u64,
    pub(crate) inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NamespaceKey {
    Abstract(UnixAddress),
    Path(UnixPathIdentity),
}

// OWNER: AF_UNIX runtime namespace 唯一拥有 abstract address / pathname inode identity 到 live
// endpoint 的解析。Weak 不延长 OFD 或 VFS inode 生命周期；register/remove_closed 都在同一
// lock 内提交，缺失时 close、unlink/recreate 与 rebind 会分裂。
static NAMESPACE: Once<Mutex<FallibleMap<NamespaceKey, Weak<UnixSocket>>>> = Once::new();

/// @description 原子注册一个 abstract address 或 pathname inode identity。
/// @param socket 尚未发布 namespace binding 的 endpoint。
/// @param key 已由 sockaddr/VFS seam 验证的 namespace identity。
/// @return namespace publication 成功。
/// @errors live collision 返回 AddressInUse，node allocation 失败返回 NoMemory。
pub(super) fn register(socket: &Arc<UnixSocket>, key: NamespaceKey) -> Result<(), SocketError> {
    let mut namespace = NAMESPACE
        .call_once(|| Mutex::new(FallibleMap::new()))
        .lock();
    namespace.retain(|_, socket| socket.strong_count() != 0);
    if namespace.contains_key(&key) {
        return Err(SocketError::AddressInUse);
    }
    let prepared =
        FallibleMap::try_prepare(key, Arc::downgrade(socket)).map_err(|_| SocketError::NoMemory)?;
    namespace.commit_vacant(prepared);
    Ok(())
}

/// @description 解析 live AF_UNIX endpoint。
/// @param key 已由 sockaddr/VFS seam 验证的 identity。
/// @return live endpoint Arc。
/// @errors 未注册或 owner 已关闭返回 NotFound。
pub(super) fn lookup(key: &NamespaceKey) -> Result<Arc<UnixSocket>, SocketError> {
    NAMESPACE
        .call_once(|| Mutex::new(FallibleMap::new()))
        .lock()
        .get(key)
        .and_then(Weak::upgrade)
        .ok_or(SocketError::NotFound)
}

/// @description 删除已经失去 endpoint owner 的 runtime binding。
/// @param key closing endpoint 曾发布的 identity。
/// @return 无返回值；若同名已换代为 live binding则保持不变。
pub(super) fn remove_closed(key: &NamespaceKey) {
    let mut namespace = NAMESPACE
        .call_once(|| Mutex::new(FallibleMap::new()))
        .lock();
    if namespace
        .get(key)
        .is_some_and(|entry| entry.strong_count() == 0)
    {
        namespace.remove(key);
    }
}
