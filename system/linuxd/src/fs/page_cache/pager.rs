//! 页缓存的 pager：内核在页缓存 VMO 缺页时投递 `PAGE_REQUEST`，这里从存储读入并补页。
//!
//! # 线程
//!
//! 一个分派线程从 port 取请求，交给工作线程处理；没有空闲工作线程时新建一个（上限 [`MAX_WORKERS`]）。
//! 存储读可能长时间阻塞（FUSE 请求等待用户态守护进程），而守护进程自身的代码页又可能需要从另一个
//! 文件系统补页：单线程 pager 在这种依赖下会死锁，因此每个阻塞的请求各占一个工作线程。
//!
//! # 补页
//!
//! 1. 完全越过 EOF 的页直接补零，不读存储；
//! 2. 其余页按 [`Inode::read_storage_pages`] 一次读入，经工作线程独占的辅助 VMO `supply_pages`；
//! 3. 请求按 [`Inode::readahead_pages`] 扩展为预读窗口；
//! 4. 读失败时以 `OP_FAIL` 撤销该区间的请求：当前访问者得到 `ERR_IO`（映射访问为 SIGBUS），之后的访问重试。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::sync::{Mutex, Once};
use native_runtime::native_abi::{self, PortPacket, packet, pager as pager_op};
use native_runtime::thread::{Descriptor, Park};
use native_runtime::{Pager, Port, Vmo};

use super::CachedFile;
use crate::memory::{MemoryObject, PAGE_SIZE};

/// 工作线程上限。
const MAX_WORKERS: usize = 64;
/// 单次存储读的最多页数。
const MAX_SUPPLY_PAGES: usize = 32;

struct Shared {
    pager: Pager,
    port: Port,
    // OWNER: pager key → 缓存文件；文件销毁后 Weak 失效，迟到的请求以 OP_FAIL 结束。
    files: Mutex<BTreeMap<u64, Weak<CachedFile>>>,
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
            files: Mutex::new(BTreeMap::new()),
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

/// 为缓存文件创建 pager VMO，返回对象与请求 key。
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
        *next += 1;
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

/// 登记 key 对应的缓存文件（发布后才能补页）。
pub(super) fn register(key: u64, file: &Arc<CachedFile>) {
    shared().files.lock().insert(key, Arc::downgrade(file));
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
        serve(shared, &request, &mut aux);
    }
}

fn serve(shared: &Shared, request: &PortPacket, aux: &mut Option<Vmo>) {
    let offset = request.payload;
    let length = u64::from(request.observed) * PAGE_SIZE as u64;
    let file = shared
        .files
        .lock()
        .get(&request.key)
        .and_then(Weak::upgrade);
    let Some(file) = file else {
        return;
    };
    let vmo = file.object.vmo();
    let mut cursor = offset;
    // 预读：把补页扩展到 filesystem 声明的窗口（FUSE 协商的 max_readahead），不越过 EOF 与对象大小。
    // 已驻留的页由内核保持不变，扩展部分只填补缺页。
    let window_end = offset.saturating_add((file.inode.readahead_pages() * PAGE_SIZE) as u64);
    let limit = file
        .inode
        .size()
        .next_multiple_of(PAGE_SIZE as u64)
        .min(vmo.size().unwrap_or(0));
    let end = (offset + length).max(window_end.min(limit));
    while cursor < end {
        let pages = ((end - cursor) / PAGE_SIZE as u64).min(MAX_SUPPLY_PAGES as u64) as usize;
        let chunk = (pages * PAGE_SIZE) as u64;
        if supply(shared, &file, cursor, pages, aux).is_err() {
            let _ = shared.pager.op(pager_op::OP_FAIL, vmo, cursor, chunk);
        }
        cursor += chunk;
    }
}

/// 读入并补入 `[offset, offset + pages * PAGE_SIZE)`。
fn supply(
    shared: &Shared,
    file: &CachedFile,
    offset: u64,
    pages: usize,
    aux: &mut Option<Vmo>,
) -> Result<(), ()> {
    let length = pages * PAGE_SIZE;
    let size = file.inode.size();
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(length).map_err(|_| ())?;
    buffer.resize(length, 0);
    if offset < size {
        let valid = ((size - offset) as usize).min(length);
        let mut slices: Vec<&mut [u8]> = Vec::new();
        slices.try_reserve_exact(pages).map_err(|_| ())?;
        let mut rest = &mut buffer[..valid];
        while !rest.is_empty() {
            let take = rest.len().min(PAGE_SIZE);
            let (head, tail) = rest.split_at_mut(take);
            slices.push(head);
            rest = tail;
        }
        let read = file
            .inode
            .read_storage_pages(offset, &mut slices)
            .map_err(|_| ())?;
        if read != valid {
            return Err(());
        }
    }
    if aux.is_none() {
        *aux = Some(Vmo::create((MAX_SUPPLY_PAGES * PAGE_SIZE) as u64, 0).map_err(|_| ())?);
    }
    let aux = aux.as_ref().expect("auxiliary VMO created above");
    aux.write(&buffer, 0).map_err(|_| ())?;
    shared
        .pager
        .supply_pages(file.object.vmo(), offset, length as u64, aux, 0)
        .map_err(|_| ())
}
