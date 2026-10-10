use alloc::sync::{Arc, Weak};
use spin::{Mutex, Once};

use crate::fallible_tree::FallibleMap;

use super::{BoundAddress, SocketError, SocketState, SocketType, UnixSocket};

const UNIX_PATH_MAX: usize = 108;

/// Linux `sockaddr_un.sun_path` 的长度保留地址值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnixAddress {
    length: u8,
    bytes: [u8; UNIX_PATH_MAX],
}

impl UnixAddress {
    /// 从不含 family 的原始 sun_path bytes 构造地址。
    ///
    /// # Parameters
    ///
    /// - `bytes`: abstract 地址包含首个 NUL；pathname 地址后续由 VFS seam 规范化。
    ///
    /// # Returns
    ///
    /// 保留原始长度的地址值。
    ///
    /// # Errors
    ///
    /// 超过 Linux 108-byte sun_path 返回 Invalid；空地址表示仅含 family 的 autobind 请求。
    pub(crate) fn new(bytes: &[u8]) -> Result<Self, SocketError> {
        if bytes.len() > UNIX_PATH_MAX {
            return Err(SocketError::Invalid);
        }
        let mut address = Self {
            length: bytes.len() as u8,
            bytes: [0; UNIX_PATH_MAX],
        };
        address.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(address)
    }

    /// 返回调用方提供的精确 sun_path bytes。
    ///
    /// # Returns
    ///
    /// 不含 sockaddr family 的地址 slice。
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.length)]
    }

    /// 判断仅含 sockaddr family 的未命名地址。
    pub(crate) fn is_unnamed(&self) -> bool {
        self.length == 0
    }

    /// 区分 Linux abstract 与 pathname namespace。
    ///
    /// # Returns
    ///
    /// sun_path 首字节为 NUL 时为 true。
    pub(crate) fn is_abstract(&self) -> bool {
        self.bytes().first() == Some(&0)
    }
}

/// VFS socket inode 的稳定 identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnixPathIdentity {
    pub(crate) filesystem: u64,
    pub(crate) inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NamespaceKey {
    Abstract(u64, UnixAddress, SocketType),
    Path(UnixPathIdentity),
}

// OWNER: AF_UNIX runtime namespace 唯一拥有 abstract address / pathname inode identity 到 live
// endpoint 的解析；abstract identity 包含实际NET id和socket type，pathname仍按真实inode全局查找。
// NET id由实际endpoint对象投影，单调不复用；Weak不延长OFD/NET/VFS生命周期。register/remove_closed都在同一
// lock 内提交，缺失时 close、unlink/recreate 与 rebind 会分裂。node 在 address/namespace guard
// 外准备；只退休当前 key 的失效 Weak，不扫描无关 endpoint，全部 node Drop 均在 guards 外。
// OWNER: 上述 namespace registry 的唯一静态根；全部修改遵循同一锁和锁外退休。
static NAMESPACE: Once<Mutex<FallibleMap<NamespaceKey, Weak<UnixSocket>>>> = Once::new();

/// 同一 namespace publication 的显式命名或 Linux 五位十六进制 autobind 请求。
#[allow(
    clippy::large_enum_variant,
    reason = "bind preparation 的有界 sockaddr 输入只在栈上传递，避免为一次冷路径输入增加可失败分配"
)]
pub(super) enum BindingRequest {
    Named {
        key: NamespaceKey,
        address: UnixAddress,
        path_owner: Option<Arc<dyn Send + Sync>>,
    },
    Automatic {
        seed: u32,
    },
}

impl BindingRequest {
    fn candidate(
        &self,
        net_id: u64,
        socket_type: SocketType,
        attempt: u32,
    ) -> (NamespaceKey, UnixAddress) {
        match self {
            Self::Named { key, address, .. } => (*key, *address),
            Self::Automatic { seed } => {
                let number = seed.wrapping_add(attempt + 1) & 0xfffff;
                let mut bytes = [0u8; 6];
                for (index, byte) in bytes[1..].iter_mut().enumerate() {
                    *byte = b"0123456789abcdef"[((number >> ((4 - index) * 4)) & 15) as usize];
                }
                let address = UnixAddress::new(&bytes).expect("fixed autobind address length");
                (
                    NamespaceKey::Abstract(net_id, address, socket_type),
                    address,
                )
            }
        }
    }
}

/// 在同一 transaction 发布显式或自动绑定。
///
/// # Parameters
/// - `socket`: 实际 endpoint owner；最终 Closed state 不可再次发表。
/// - `request`: 显式 namespace identity，或自动名称搜索的起始 seed。
///
/// # Returns
/// 成功提交 binding；重复自动绑定保留原地址并成功，不分配节点。
///
/// # Errors
/// 显式碰撞 AddressInUse，自动名称空间耗尽 NoSpace，节点 OOM NoMemory，
/// 已关闭或重复显式绑定 Invalid；失败不改变 endpoint 或 registry。
pub(super) fn register(
    socket: &Arc<UnixSocket>,
    mut request: BindingRequest,
) -> Result<(), SocketError> {
    let automatic = matches!(request, BindingRequest::Automatic { .. });
    if automatic {
        let bound = socket.address.lock();
        let state = socket.state.lock();
        if matches!(*state, SocketState::Closed) {
            return Err(SocketError::Invalid);
        }
        if bound.is_some() {
            return Ok(());
        }
    }
    // 1. 一个节点在全部 owner guards 外准备；碰撞搜索复用同一 storage。
    let (key, _) = request.candidate(socket.network_namespace_id(), socket.socket_type, 0);
    let mut prepared =
        FallibleMap::try_prepare(key, Arc::downgrade(socket)).map_err(|_| SocketError::NoMemory)?;
    let attempts = if automatic { 1 << 20 } else { 1 };
    for attempt in 0..attempts {
        let (key, address) =
            request.candidate(socket.network_namespace_id(), socket.socket_type, attempt);
        let mut bound = socket.address.lock();
        let mut namespace = NAMESPACE
            .call_once(|| Mutex::new(FallibleMap::new()))
            .lock();
        // 2. namespace → state 顺序阻止 close 与 binding commit 交错；close 在取 address
        // 前已释放 state。碰撞 endpoint 的 state 从不在等待 namespace 时持有。
        let state = socket.state.lock();
        if matches!(*state, SocketState::Closed) {
            return Err(SocketError::Invalid);
        }
        if bound.is_some() {
            return if automatic {
                Ok(())
            } else {
                Err(SocketError::Invalid)
            };
        }
        let collision = namespace.get(&key).and_then(Weak::upgrade);
        let occupied = collision
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(entry, socket) || !entry.is_closed());
        if occupied {
            drop(state);
            drop(namespace);
            drop(bound);
            drop(collision);
            if automatic {
                continue;
            }
            return Err(SocketError::AddressInUse);
        }
        // 3. AVL membership 与 getsockname identity 一次提交；旧 node/Arc 锁外退休。
        let retired = namespace.take_entry(&key);
        prepared.set_key(key);
        namespace.commit_vacant(prepared);
        *bound = Some(BoundAddress {
            visible: address,
            binding: Some(key),
            path_owner: match &mut request {
                BindingRequest::Named { path_owner, .. } => path_owner.take(),
                BindingRequest::Automatic { .. } => None,
            },
        });
        drop(state);
        drop(namespace);
        drop(bound);
        drop(retired);
        drop(collision);
        return Ok(());
    }
    Err(SocketError::NoSpace)
}

/// 解析 live AF_UNIX endpoint。
///
/// # Parameters
///
/// - `key`: 已由 sockaddr/VFS seam 验证的 identity。
///
/// # Returns
///
/// live endpoint Arc。
///
/// # Errors
///
/// 未注册或 owner 已关闭返回 ConnectionRefused。
pub(super) fn lookup(key: &NamespaceKey) -> Result<Arc<UnixSocket>, SocketError> {
    let endpoint = NAMESPACE
        .call_once(|| Mutex::new(FallibleMap::new()))
        .lock()
        .get(key)
        .cloned()
        .and_then(|entry| entry.upgrade());
    endpoint
        .filter(|endpoint| !endpoint.is_closed())
        .ok_or(SocketError::ConnectionRefused)
}

/// 删除已经失去 endpoint owner 的 runtime binding。
///
/// # Parameters
///
/// - `key`: closing endpoint 曾发布的 identity。
///
/// # Returns
///
/// 无返回值；若同名已换代为 live binding则保持不变。
pub(super) fn remove_closed(key: &NamespaceKey) {
    let mut namespace = NAMESPACE
        .call_once(|| Mutex::new(FallibleMap::new()))
        .lock();
    let endpoint = namespace.get(key).and_then(Weak::upgrade);
    let retired = if endpoint
        .as_ref()
        .is_none_or(|endpoint| endpoint.is_closed())
    {
        namespace.take_entry(key)
    } else {
        None
    };
    drop(namespace);
    drop(retired);
    drop(endpoint);
}
