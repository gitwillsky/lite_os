//! 网络域的实际 namespace policy；socket 捕获对象，控制消息不从当前 Task 重新取 policy。
use crate::namespace::{NamespaceAuthority, NamespaceError, UserNamespace};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU32, Ordering};
use spin::{Mutex, Once};

// 每次safe point最多观察8个域；cursor保留严格后继位置，不从initial域重复开始。
// 每域仍有64-frame/cleanup/notification budget，因此NET数量不增加单轮无界延迟。
const NETWORK_NAMESPACE_BUDGET: usize = 8;

/// deferred与timer只读域目录的独立遍历目的；不参与socket操作的owner解析。
pub(in crate::socket) enum NetworkNamespaceScan {
    Deferred,
    Timer,
}

struct NetworkNamespaceDirectory {
    // OWNER: 仅索引实际域的Weak；不保活namespace、协议、端口或ucounts。
    entries: crate::fallible_tree::FallibleMap<u64, Weak<NetNamespace>>,
    // OWNER: 每个观察者唯一fairness cursor；不复制协议deadline或ready state。
    deferred_cursor: u64,
    timer_cursor: u64,
}
static NETWORK_NAMESPACES: Mutex<NetworkNamespaceDirectory> =
    Mutex::new(NetworkNamespaceDirectory {
        entries: crate::fallible_tree::FallibleMap::new(),
        deferred_cursor: 0,
        timer_cursor: 0,
    });

/// 目录锁外保活本轮实际域；最后Arc及其protocol/ucounts只在所有guard外退休。
pub(in crate::socket) struct NetworkNamespaceBatch {
    pub(in crate::socket) namespaces: [Option<Arc<NetNamespace>>; NETWORK_NAMESPACE_BUDGET],
    pub(in crate::socket) backlog: bool,
}

/// 零分配、有界取出下一批实际域；竞争返回None，由caller回投而不等待或spin。
pub(in crate::socket) fn take_network_namespaces(
    scan: NetworkNamespaceScan,
) -> Option<NetworkNamespaceBatch> {
    let mut directory = NETWORK_NAMESPACES.try_lock()?;
    let mut cursor = match scan {
        NetworkNamespaceScan::Deferred => directory.deferred_cursor,
        NetworkNamespaceScan::Timer => directory.timer_cursor,
    };
    let mut batch = NetworkNamespaceBatch {
        namespaces: core::array::from_fn(|_| None),
        backlog: false,
    };
    for slot in &mut batch.namespaces {
        let Some((&id, namespace)) = directory.entries.successor(&cursor) else {
            cursor = 0;
            break;
        };
        cursor = id;
        *slot = namespace.upgrade();
    }
    if cursor != 0 {
        batch.backlog = directory.entries.successor(&cursor).is_some();
        if !batch.backlog {
            cursor = 0;
        }
    }
    match scan {
        NetworkNamespaceScan::Deferred => directory.deferred_cursor = cursor,
        NetworkNamespaceScan::Timer => directory.timer_cursor = cursor,
    }
    drop(directory);
    Some(batch)
}

fn publish(namespace: Arc<NetNamespace>) -> Result<Arc<NetNamespace>, NamespaceError> {
    // 1. namespace尚未移交Task；node OOM先退休Arc并退款，不留下半发布的目录项。
    let entry =
        crate::fallible_tree::FallibleMap::try_prepare(namespace.id, Arc::downgrade(&namespace))
            .map_err(|_| NamespaceError::OutOfMemory)?;
    // 2. 唯一单调id的vacant commit不分配，目录不保存第二份protocol状态。
    NETWORK_NAMESPACES.lock().entries.commit_vacant(entry);
    Ok(namespace)
}

/// 网络域 owning UserNS 与 optmem policy；initial owner 在用户 Task 构造前装配。
pub(crate) struct NetNamespace {
    id: u64,
    user: Arc<UserNamespace>,
    // OWNER: Linux net.core.sysctl_optmem_max 的唯一 scalar；不复制到各个 socket。
    optmem_max: AtomicU32,
    // OWNER: 同一网络域唯一协议owner；boot在设备装配后安装，之后不替换或复制其SocketSet/port状态。
    protocol: Once<Arc<super::inet::NetworkStackOwner>>,
    _charge: Option<crate::namespace::UserResourceCharge>,
}
impl core::fmt::Debug for NetNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetNamespace").finish_non_exhaustive()
    }
}
impl NetNamespace {
    /// 准备新网络域，固定独立optmem初值并对创建者/祖先实际ucounts收费；失败全部退款。
    /// transport与接口装配在task membership publication前完成，不复制旧网络域的协议状态。
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
        publish(namespace)
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(in crate::socket) fn protocol(&self) -> Option<&super::inet::NetworkStackOwner> {
        self.protocol.get().map(Arc::as_ref)
    }
    /// 只移交完整准备的实际owner；没有allocation、Task publication或第二份协议状态。
    pub(in crate::socket) fn install_protocol(&self, owner: Arc<super::inet::NetworkStackOwner>) {
        self.protocol.call_once(|| owner);
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
            || authority.capable(&self.user, syscall_abi::capability::Capability::NET_ADMIN)
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
impl Drop for NetNamespace {
    fn drop(&mut self) {
        // 最后实际owner先摘目录，node/Weak及后续protocol/ucounts析构全部在guard外。
        // 若准备在node OOM失败，take_entry为空；没有补偿副计数或全表扫描。
        let retired = NETWORK_NAMESPACES.lock().entries.take_entry(&self.id);
        drop(retired);
    }
}
// OWNER: boot 唯一 initial NET policy；不依赖 NIC 发现，离线 AF_UNIX 也持同一对象。
static INITIAL_NET: Once<Arc<NetNamespace>> = Once::new();
pub(crate) fn initialize_net() -> Result<(), NamespaceError> {
    // Initial identity 6 只能由 Once 的成功初始化事务登记；并发 caller 不得先发布重复 key。
    INITIAL_NET
        .try_call_once(|| {
            let initial = Arc::try_new(NetNamespace {
                id: 6,
                user: crate::namespace::initial_user(),
                optmem_max: AtomicU32::new(128 * 1024),
                protocol: Once::new(),
                _charge: None,
            })
            .map_err(|_| NamespaceError::OutOfMemory)?;
            publish(initial)
        })
        .map(|_| ())
}
pub(crate) fn initial_net() -> Arc<NetNamespace> {
    INITIAL_NET
        .get()
        .expect("NET initialization precedes Task creation")
        .clone()
}
pub(in crate::socket) fn initial_net_ref() -> &'static NetNamespace {
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
