//! 网络域的实际 namespace policy；socket 捕获对象，控制消息不从当前 Task 重新取 policy。
//! 协议状态（接口、FIB、endpoint）在 netstack 的对应实例中，经域 channel 访问。
use crate::namespace::{NamespaceAuthority, NamespaceError, UserNamespace};
use crate::sync::Once;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};
use network_protocol::RemoteNamespace;

/// 网络域 owning UserNS 与 optmem policy；initial owner 在用户 Task 构造前装配。
pub(crate) struct NetNamespace {
    id: u64,
    user: Arc<UserNamespace>,
    // OWNER: Linux net.core.sysctl_optmem_max 的唯一 scalar；不复制到各个 socket。
    optmem_max: AtomicU32,
    // OWNER: 同一网络域在 netstack 中唯一协议实例的域 channel；boot 在设备装配后安装，新域在发布前安装，
    // 之后不替换。随最后一个引用关闭，netstack 据此退休该实例。
    protocol: Once<RemoteNamespace>,
    _charge: Option<crate::namespace::UserResourceCharge>,
}
impl core::fmt::Debug for NetNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetNamespace").finish_non_exhaustive()
    }
}
impl NetNamespace {
    /// 标准 NEWNET 的完整准备：固定独立 optmem 初值并对创建者/祖先实际 ucounts 收费，在 netstack 中创建
    /// 只有 DOWN 且无地址回环的协议实例；任一失败全部退款，不复制旧网络域的协议状态。
    pub(crate) fn create(
        user: Arc<UserNamespace>,
        creator_uid: u32,
    ) -> Result<Arc<Self>, NamespaceError> {
        let charge = crate::namespace::UserResourceCharge::acquire(
            &user,
            creator_uid,
            crate::namespace::UserResource::Net,
        )?;
        let namespace = Arc::try_new(Self {
            id: crate::namespace::next_namespace_id(),
            user,
            optmem_max: AtomicU32::new(128 * 1024),
            protocol: Once::new(),
            _charge: Some(charge),
        })
        .map_err(|_| NamespaceError::OutOfMemory)?;
        let protocol = super::netstack::create_namespace().map_err(|error| match error {
            super::SocketError::NoMemory => NamespaceError::OutOfMemory,
            _ => NamespaceError::Invalid,
        })?;
        namespace.install_protocol(protocol);
        Ok(namespace)
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(in crate::net) fn protocol(&self) -> Option<&RemoteNamespace> {
        self.protocol.get()
    }
    /// 只安装一次；之后不替换协议实例。
    pub(in crate::net) fn install_protocol(&self, protocol: RemoteNamespace) {
        self.protocol.call_once(|| protocol);
    }
    pub(crate) fn is_initial(&self) -> bool {
        self.id == 6
    }
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
    pub(crate) fn optmem_max(&self) -> usize {
        self.optmem_max.load(Ordering::Relaxed) as usize
    }
    pub(crate) fn may_set_limits(&self, authority: &NamespaceAuthority) -> bool {
        // net_ctl_permissions放大NET_ADMIN的mode；proc_sysctl::test_perm仍检查GLOBAL_ROOT_UID，
        // 不用stat显示的namespace-root kuid作为额外write豁免。
        authority.effective_uid == 0
            || authority.capable(&self.user, linux_abi::capability::Capability::NET_ADMIN)
    }
    /// 原生 unsigned minmax 接受 0..INT_MAX；缩小不撤销已收费 allocation，下一次 admission 用新值。
    pub(crate) fn set_optmem_max(
        &self,
        value: u32,
        authority: &NamespaceAuthority,
    ) -> Result<(), NamespaceError> {
        if !self.may_set_limits(authority) {
            return Err(NamespaceError::Permission);
        }
        if value > i32::MAX as u32 {
            return Err(NamespaceError::Invalid);
        }
        self.optmem_max.store(value, Ordering::Relaxed);
        Ok(())
    }
}
// OWNER: boot 唯一 initial NET policy；不依赖 NIC 发现，离线 AF_UNIX 也持同一对象。
static INITIAL_NET: Once<Arc<NetNamespace>> = Once::new();
pub(crate) fn initialize_net() -> Result<(), NamespaceError> {
    // Initial identity 6 只能由 Once 的成功初始化事务登记；并发 caller 不得先发布重复 key。
    INITIAL_NET
        .try_call_once(|| {
            Arc::try_new(NetNamespace {
                id: 6,
                user: crate::namespace::initial_user(),
                optmem_max: AtomicU32::new(128 * 1024),
                protocol: Once::new(),
                _charge: None,
            })
            .map_err(|_| NamespaceError::OutOfMemory)
        })
        .map(drop)
}
pub(crate) fn initial_net() -> Arc<NetNamespace> {
    INITIAL_NET
        .get()
        .expect("NET initialization precedes Task creation")
        .clone()
}
pub(in crate::net) fn initial_net_ref() -> &'static NetNamespace {
    INITIAL_NET
        .get()
        .expect("NET initialization precedes protocol/device assembly")
        .as_ref()
}

impl crate::namespace::NamespaceOwner for NetNamespace {
    fn kind(&self) -> crate::namespace::NamespaceType {
        crate::namespace::NamespaceType::Net
    }
    fn id(&self) -> u64 {
        self.id
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.user.clone())
    }
}
