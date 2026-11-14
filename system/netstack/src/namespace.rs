//! 网络域：每个域独占一个协议 owner（接口、FIB、socket 集合与端口空间）。
//!
//! 域由使用者经控制 channel 创建；域 channel 与该域的 socket 共同保活它，最后一个引用释放时退出目录。
//! 协议推进线程按有界批次遍历目录，不从域对象反向保活协议状态。

use alloc::sync::{Arc, Weak};

use native_runtime::sync::Once;

use crate::inet::NetworkStackOwner;
use crate::sync::Mutex;

// 每轮最多观察8个域；cursor保留严格后继位置，不从首个域重复开始。
// 每域仍有64-frame/cleanup/notification budget，因此域数量不增加单轮无界延迟。
const NETWORK_NAMESPACE_BUDGET: usize = 8;

/// 推进线程自己的有界游标。当前线程访问固定分区按执行线程数取余后属于自己的域。
pub(crate) struct DomainScan {
    lane: usize,
    lanes: usize,
    after: (usize, u64),
}

impl DomainScan {
    /// 从线程 `lane` 的首个分区开始，以 `lanes` 为步长。
    pub(crate) fn new(lane: usize, lanes: usize) -> Self {
        Self {
            lane,
            lanes,
            after: (lane, 0),
        }
    }

    /// 时间预算用完时，从最后一个已服务的域继续，而不是跳过本批的剩余域。
    pub(crate) fn resume_after(&mut self, domain: u64) {
        let partition = crate::worker::partition(domain);
        debug_assert_eq!(partition % self.lanes, self.lane);
        self.after = (partition, domain);
    }

    fn next<'a>(
        &self,
        entries: &'a fallible_tree::FallibleMap<(usize, u64), Weak<Namespace>>,
    ) -> Option<(&'a (usize, u64), &'a Weak<Namespace>)> {
        let mut after = self.after;
        loop {
            let entry = entries.successor(&after)?;
            let partition = entry.0.0;
            let offset = (partition - self.lane) % self.lanes;
            if offset == 0 {
                return Some(entry);
            }
            // 域 ID 从 1 开始，直接跳到下一相关分区，跳过其他线程的所有域。
            after = (partition + self.lanes - offset, 0);
        }
    }
}

struct NetworkNamespaceDirectory {
    // OWNER: 按固定分区和域 ID 排序，只持弱引用。独占核切换时不迁移索引或协议状态。
    entries: fallible_tree::FallibleMap<(usize, u64), Weak<Namespace>>,
    // OWNER: 单调域标识；packet socket 的 membership key 不复用。
    next_id: u64,
}

static NETWORK_NAMESPACES: Mutex<NetworkNamespaceDirectory> =
    Mutex::new(NetworkNamespaceDirectory {
        entries: fallible_tree::FallibleMap::new(),
        next_id: 1,
    });

/// 目录锁外保活本轮实际域；最后 Arc 及协议在所有 guard 外退休。
pub(crate) struct NetworkNamespaceBatch {
    pub(crate) namespaces: [Option<Arc<Namespace>>; NETWORK_NAMESPACE_BUDGET],
    pub(crate) backlog: bool,
}

/// 零分配、有界取得同一执行分区的下一批域。目录争用时返回 None 供调用者稍后重试。
pub(crate) fn take_network_namespaces(scan: &mut DomainScan) -> Option<NetworkNamespaceBatch> {
    let directory = NETWORK_NAMESPACES.try_lock()?;
    let mut batch = NetworkNamespaceBatch {
        namespaces: core::array::from_fn(|_| None),
        backlog: false,
    };
    for slot in &mut batch.namespaces {
        let Some((&key, namespace)) = scan.next(&directory.entries) else {
            break;
        };
        scan.after = key;
        *slot = namespace.upgrade();
    }
    batch.backlog = scan.next(&directory.entries).is_some();
    if !batch.backlog {
        scan.after = (scan.lane, 0);
    }
    drop(directory);
    Some(batch)
}

/// 一个网络域。
pub(crate) struct Namespace {
    id: u64,
    // OWNER: 本域唯一抓包目录。设备可共享它但不能反向保活域或其协议栈。
    packets: Arc<crate::packet::PacketRegistry>,
    // OWNER: 创建时决定直接配置权。托管域只由安装能力发布事务，域连接不获写权。
    configuration: ConfigurationOwner,
    // OWNER: 创建网络域时安装唯一协议栈，协议端点与端口状态随域存续。
    protocol: Once<Arc<NetworkStackOwner>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigurationOwner {
    Namespace,
    Installer,
}

impl core::fmt::Debug for Namespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Namespace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Namespace {
    /// 分配标识并登记到目录；协议 owner 由调用者随后安装。
    ///
    /// # Errors
    ///
    /// 分配失败。
    pub(crate) fn create() -> Result<Arc<Self>, ()> {
        Self::create_with_configuration(ConfigurationOwner::Namespace)
    }

    /// 创建由安装能力管理的网络域，普通域连接只能观察和收发。
    /// # Errors
    /// 分配失败。
    pub(crate) fn create_managed() -> Result<Arc<Self>, ()> {
        Self::create_with_configuration(ConfigurationOwner::Installer)
    }

    fn create_with_configuration(configuration: ConfigurationOwner) -> Result<Arc<Self>, ()> {
        let mut directory = NETWORK_NAMESPACES.lock();
        let id = directory.next_id;
        directory.next_id = id.checked_add(1).ok_or(())?;
        drop(directory);
        crate::worker::ensure_partition(id).map_err(|_| ())?;
        let namespace = Arc::try_new(Self {
            id,
            packets: Arc::try_new(crate::packet::PacketRegistry::new()).map_err(|_| ())?,
            configuration,
            protocol: Once::new(),
        })
        .map_err(|_| ())?;
        // 1. 唯一单调 id 的目录节点在锁外准备；OOM 先退休 Arc，不留下半发布的目录项。
        let entry = fallible_tree::FallibleMap::try_prepare(
            (crate::worker::partition(id), id),
            Arc::downgrade(&namespace),
        )
        .map_err(|_| ())?;
        // 2. vacant commit 不分配，目录不保存第二份protocol状态。
        NETWORK_NAMESPACES.lock().entries.commit_vacant(entry);
        Ok(namespace)
    }

    /// 普通域连接能否修改接口与路由。安装事务拥有单独的授权入口。
    pub(crate) fn permits_direct_configuration(&self) -> bool {
        self.configuration == ConfigurationOwner::Namespace
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// 本域唯一抓包目录。接口安装时可克隆引用，收发只借用目录。
    pub(crate) fn packets(&self) -> &Arc<crate::packet::PacketRegistry> {
        &self.packets
    }

    pub(crate) fn protocol(&self) -> Option<&NetworkStackOwner> {
        self.protocol.get().map(Arc::as_ref)
    }

    /// 为新设备准备本域的链路通知及安装者观察回调。
    /// # Errors
    /// 协议尚未安装或回调分配失败。
    pub(crate) fn link_activity(
        &self,
        observer: impl Fn() + Send + Sync + 'static,
    ) -> Result<device_model::Activity, network_protocol::SocketError> {
        self.protocol
            .get()
            .ok_or(network_protocol::SocketError::NetworkUnreachable)?
            .link_activity(self.id, observer)
    }

    /// 只移交完整准备的实际owner；没有allocation或第二份协议状态。
    pub(crate) fn install_protocol(&self, owner: Arc<NetworkStackOwner>) {
        self.protocol.call_once(|| owner);
        crate::worker::request_poll(self.id);
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        // 最后实际owner先摘目录，node/Weak及后续protocol析构全部在guard外。
        let retired = NETWORK_NAMESPACES
            .lock()
            .entries
            .take_entry(&(crate::worker::partition(self.id), self.id));
        drop(retired);
    }
}
