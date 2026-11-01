//! 文件内容 pager：共用一个 native pager、port、注册表和可增长工作池。
//!
//! # 线程
//!
//! 一个分派线程从 port 取请求，交给工作线程处理；没有空闲工作线程时新建一个（上限 [`MAX_WORKERS`]）。
//! 存储读可能长时间阻塞（FUSE 请求等待用户态守护进程），而守护进程自身的代码页又可能需要从另一个
//! 文件系统补页：单线程 pager 在这种依赖下会死锁，因此每个阻塞的请求各占一个工作线程。
//!
//! PageSource 拥有补页政策：持久文件从存储读入，内存文件在实际页提交时检查配额。
//! 内核只请求/接收页；文件长度、存储限额和失败语义均由 linuxd 的对应 owner 决定。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use fallible_tree::FallibleMap;

use crate::sync::{Mutex, Once};
use native_runtime::native_abi::{self, PortPacket, packet, pager as pager_op};
use native_runtime::thread::{Descriptor, Park};
use native_runtime::{Pager, Port, Vmo};

use crate::memory::{MemoryObject, PAGE_SIZE};

/// 工作线程上限。
const MAX_WORKERS: usize = 64;

/// 文件内容的唯一补页政策；调用者不持 native VMO 锁。
/// 每个请求必须完成 supply 或 OP_FAIL，不得把等待者留在未完成状态。
pub(super) trait PageSource: Send + Sync {
    /// 为请求的字节区间补页；aux 是本工作线程复用的可移交 VMO。
    fn page_request(&self, pager: &Pager, offset: u64, length: u64, aux: &mut Option<Vmo>);
}

struct Shared {
    pager: Pager,
    port: Port,
    // OWNER: pager key → 缓存文件；文件销毁后 Weak 失效，迟到的请求以 OP_FAIL 结束。
    files: Mutex<FallibleMap<u64, Weak<dyn PageSource>>>,
    next_key: Mutex<u64>,
    // OWNER: 待处理请求与空闲工作线程；分派线程与工作线程之间的唯一交接点。
    queue: Mutex<Queue>,
}

struct Queue {
    requests: VecDeque<PortPacket>,
    idle: Vec<Arc<Descriptor>>,
    workers: usize,
}

// OWNER: 本进程唯一文件 pager、key 注册表与工作队列，由 Once 完整发布。
static SHARED: Once<Shared> = Once::new();

fn shared() -> &'static Shared {
    SHARED.call_once(|| {
        let shared = Shared {
            pager: Pager::create().expect("page-cache pager creation"),
            port: Port::create().expect("page-cache pager port"),
            files: Mutex::new(FallibleMap::new()),
            next_key: Mutex::new(1),
            queue: Mutex::new(Queue {
                requests: VecDeque::new(),
                idle: Vec::new(),
                workers: 0,
            }),
        };
        native_runtime::thread::spawn(b"pager", dispatch).expect("pager dispatch thread");
        shared
    })
}

/// 为文件内容创建可增长 pager VMO，返回对象与唯一请求 key。
///
/// # Errors
///
/// VMO 创建失败。
pub(super) fn create_object(
    size: u64,
) -> Result<(Arc<MemoryObject>, u64), crate::fs::FileSystemError> {
    let shared = shared();
    let key = {
        let mut next = shared.next_key.lock();
        let key = *next;
        *next = key.checked_add(1).expect("file pager key exhausted");
        key
    };
    let vmo = shared
        .pager
        .create_vmo(
            native_abi::vmo::RESIZABLE,
            &shared.port,
            key,
            size.next_multiple_of(PAGE_SIZE as u64),
        )
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    let object = MemoryObject::new(vmo).map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    Ok((object, key))
}

/// 发布 key 对应的内容 owner；只能在对象完成构造后调用。
/// 注册失败返回 OutOfMemory，尚未发表的对象仍由调用者回滚。
pub(super) fn register<T: PageSource + 'static>(
    key: u64,
    file: &Arc<T>,
) -> Result<(), crate::fs::FileSystemError> {
    let source: Arc<dyn PageSource> = file.clone();
    shared()
        .files
        .lock()
        .try_insert(key, Arc::downgrade(&source))
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    Ok(())
}

/// 同步写入与后台补页共用的 native pager；句柄只属于此模块，不转移所有权。
pub(super) fn handle() -> &'static Pager {
    &shared().pager
}

/// 注销 key。
pub(super) fn unregister(key: u64) {
    shared().files.lock().remove(&key);
}

/// 清除区间的脏标记并写保护映射（回写开始）。
pub(super) fn begin_writeback(object: &MemoryObject, offset: u64, length: u64) {
    // 只在区间越界时失败，调用者已按对象大小裁剪。
    let _ = shared()
        .pager
        .op(pager_op::OP_WRITEBACK_BEGIN, object.vmo(), offset, length);
}

/// `[offset, offset + length)` 中第一段脏区间。
pub(super) fn first_dirty(object: &MemoryObject, offset: u64, length: u64) -> Option<(u64, u64)> {
    shared()
        .pager
        .op(pager_op::OP_QUERY_DIRTY, object.vmo(), offset, length)
        .ok()
        .filter(|&(_, length)| length != 0)
}

fn dispatch() {
    let shared = shared();
    loop {
        let Ok(packet) = shared.port.wait(native_abi::DEADLINE_INFINITE) else {
            continue;
        };
        if packet.kind != packet::PAGE_REQUEST {
            continue;
        }
        let mut queue = shared.queue.lock();
        queue.requests.push_back(packet);
        if let Some(worker) = queue.idle.pop() {
            drop(queue);
            worker.unpark();
        } else if queue.workers < MAX_WORKERS {
            queue.workers += 1;
            drop(queue);
            if native_runtime::thread::spawn(b"pager-worker", work).is_err() {
                shared.queue.lock().workers -= 1;
            }
        }
    }
}

fn work() {
    let shared = shared();
    let me = native_runtime::thread::current_arc();
    let mut aux: Option<Vmo> = None;
    loop {
        let request = {
            let mut queue = shared.queue.lock();
            match queue.requests.pop_front() {
                Some(request) => request,
                None => {
                    queue.idle.push(me.clone());
                    drop(queue);
                    while me.park(native_abi::DEADLINE_INFINITE) != Park::Unparked {}
                    continue;
                }
            }
        };
        let source = shared
            .files
            .lock()
            .get(&request.key)
            .and_then(Weak::upgrade);
        if let Some(source) = source {
            source.page_request(
                &shared.pager,
                request.payload,
                u64::from(request.observed) * PAGE_SIZE as u64,
                &mut aux,
            );
        }
    }
}
