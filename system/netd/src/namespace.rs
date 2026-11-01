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

/// 协议推进与定时检查只读域目录的独立遍历目的；不参与socket操作的owner解析。
pub(crate) enum NetworkNamespaceScan {
    Deferred,
    Timer,
}

struct NetworkNamespaceDirectory {
    // OWNER: 仅索引实际域的Weak；不保活namespace或协议。
    entries: fallible_tree::FallibleMap<u64, Weak<Namespace>>,
    // OWNER: 每个观察者唯一fairness cursor；不复制协议deadline或ready state。
    deferred_cursor: u64,
    timer_cursor: u64,
    // OWNER: 单调域标识；packet socket 的 membership key 不复用。
    next_id: u64,
}

static NETWORK_NAMESPACES: Mutex<NetworkNamespaceDirectory> =
    Mutex::new(NetworkNamespaceDirectory {
        entries: fallible_tree::FallibleMap::new(),
        deferred_cursor: 0,
        timer_cursor: 0,
        next_id: 1,
    });

/// 目录锁外保活本轮实际域；最后Arc及其protocol只在所有guard外退休。
pub(crate) struct NetworkNamespaceBatch {
    pub(crate) namespaces: [Option<Arc<Namespace>>; NETWORK_NAMESPACE_BUDGET],
    pub(crate) backlog: bool,
}

/// 零分配、有界取出下一批实际域；竞争返回None，由caller重试而不等待或spin。
pub(crate) fn take_network_namespaces(scan: NetworkNamespaceScan) -> Option<NetworkNamespaceBatch> {
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

/// 一个网络域。
pub(crate) struct Namespace {
    id: u64,
    // OWNER: 同一网络域唯一协议owner；创建时安装，之后不替换或复制其SocketSet/port状态。
    protocol: Once<Arc<NetworkStackOwner>>,
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
        let mut directory = NETWORK_NAMESPACES.lock();
        let id = directory.next_id;
        directory.next_id = id.checked_add(1).ok_or(())?;
        drop(directory);
        let namespace = Arc::try_new(Self {
            id,
            protocol: Once::new(),
        })
        .map_err(|_| ())?;
        // 1. 唯一单调 id 的目录节点在锁外准备；OOM 先退休 Arc，不留下半发布的目录项。
        let entry = fallible_tree::FallibleMap::try_prepare(id, Arc::downgrade(&namespace))
            .map_err(|_| ())?;
        // 2. vacant commit 不分配，目录不保存第二份protocol状态。
        NETWORK_NAMESPACES.lock().entries.commit_vacant(entry);
        Ok(namespace)
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn protocol(&self) -> Option<&NetworkStackOwner> {
        self.protocol.get().map(Arc::as_ref)
    }

    /// 只移交完整准备的实际owner；没有allocation或第二份协议状态。
    pub(crate) fn install_protocol(&self, owner: Arc<NetworkStackOwner>) {
        self.protocol.call_once(|| owner);
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        // 最后实际owner先摘目录，node/Weak及后续protocol析构全部在guard外。
        let retired = NETWORK_NAMESPACES.lock().entries.take_entry(&self.id);
        drop(retired);
    }
}
