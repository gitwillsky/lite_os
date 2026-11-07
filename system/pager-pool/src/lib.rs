#![no_std]

//! 文件内容 pager：一个进程内共用的 native pager、port、key 注册表与可增长工作池。
//!
//! # 线程
//!
//! 一个分派线程从 port 取请求，交给工作线程处理；没有空闲工作线程时新建一个（上限 [`MAX_WORKERS`]）。
//! 存储读可能长时间阻塞（等待另一服务），而那个服务自身的代码页又可能需要从另一个文件系统补页：
//! 单线程 pager 在这种依赖下会死锁，因此每个阻塞的请求各占一个工作线程。
//!
//! 使用者是持有文件内容的服务（ext4 的文件、linuxd 的 tmpfs 与 Linux FUSE 守护进程的文件）：
//! [`PageSource`] 拥有补页政策，内核只请求/接收页。

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use fallible_tree::FallibleMap;
use native_runtime::native_abi::{self, PortPacket, Status, packet, pager as pager_op};
use native_runtime::sync::{Mutex, Once};
use native_runtime::thread::{Descriptor, Park};
use native_runtime::{Pager, Port, Vmo};

/// 工作线程上限。
const MAX_WORKERS: usize = 64;

/// 文件内容的补页政策；调用者不持 native VMO 锁。
/// 每个请求必须完成 supply 或 `OP_FAIL`，不得把等待者留在未完成状态。
pub trait PageSource: Send + Sync {
    /// 为请求的字节区间补页；`aux` 是本工作线程复用的可移交 VMO。
    fn page_request(&self, pager: &Pager, offset: u64, length: u64, aux: &mut Option<Vmo>);
}

struct Shared {
    pager: Pager,
    port: Port,
    // OWNER: pager key → 内容 owner；owner 退休时先 detach 再移除（[`retire`]），之后迟到的请求可以丢弃。
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
            pager: Pager::create().expect("file pager creation"),
            port: Port::create().expect("file pager port"),
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

/// 创建可增长的 pager VMO（大小按页取整），返回对象与唯一请求 key。
///
/// # Parameters
///
/// - `size`: 内容长度。
/// - `options`: native VMO 选项；内存内容 owner 指定 `PAGER_ANONYMOUS`，存储内容 owner 传 0。
///   该事实必须来自实际内容提供者，不能按进程名或路径推断。
///
/// # Errors
///
/// VMO 创建失败。
pub fn create_vmo(size: u64, options: u32) -> Result<(Vmo, u64), Status> {
    let shared = shared();
    let key = {
        let mut next = shared.next_key.lock();
        let key = *next;
        *next = key.checked_add(1).expect("file pager key exhausted");
        key
    };
    let vmo = shared.pager.create_vmo(
        native_abi::vmo::RESIZABLE | options,
        &shared.port,
        key,
        size.next_multiple_of(native_abi::PAGE_SIZE as u64),
    )?;
    Ok((vmo, key))
}

/// 发布 key 对应的内容 owner；只能在 owner 完成构造后调用。
///
/// # Errors
///
/// 注册表分配失败。
pub fn register(key: u64, source: Weak<dyn PageSource>) -> Result<(), Status> {
    shared()
        .files
        .lock()
        .try_insert(key, source)
        .map(drop)
        .map_err(|_| native_abi::ERR_NO_MEMORY)
}

/// 内容 owner 退休：先让 pager 与 VMO 分离（已发出的请求以失败结束，之后的缺页直接失败），再注销 key。
///
/// 仍存活的 VMO 副本（导出给使用者的映射、COW 子对象）因此不会把请求投给已不处理它的服务；
/// 分离之后迟到的请求 packet 查不到 key，可安全忽略。缺失分离时这些请求被静默丢弃，等待者永久睡眠。
pub fn retire(key: u64, vmo: &Vmo) {
    // 只在 VMO 不由本 pager 支撑时失败，此时它本来就不会产生请求。
    let _ = shared().pager.op(pager_op::OP_DETACH, vmo, 0, 0);
    shared().files.lock().remove(&key);
}

/// 同步写入与后台补页共用的 native pager；句柄只属于本 crate，不转移所有权。
pub fn handle() -> &'static Pager {
    &shared().pager
}

/// 回写开始：区间内的脏页转为待确认并写保护映射，之后的写入重新置脏。区间越界时不做任何事：
/// 调用者已按对象大小裁剪。读出内容并写入存储成功后必须调用 [`end_writeback`]。
pub fn begin_writeback(vmo: &Vmo, offset: u64, length: u64) {
    let _ = shared()
        .pager
        .op(pager_op::OP_WRITEBACK_BEGIN, vmo, offset, length);
}

/// 回写完成：存储已确认区间内容，仍待确认的页成为干净页，内核在内存紧张时可以淘汰它们。
/// 缺失时页永远停在待确认状态而不可回收；在写入存储之前调用则会让淘汰丢掉未落盘的修改。
pub fn end_writeback(vmo: &Vmo, offset: u64, length: u64) {
    let _ = shared()
        .pager
        .op(pager_op::OP_WRITEBACK_END, vmo, offset, length);
}

/// 区间内容已经与存储一致（直写之后同步进缓存）：一次完成回写的开始与结束。
pub fn mark_clean(vmo: &Vmo, offset: u64, length: u64) {
    begin_writeback(vmo, offset, length);
    end_writeback(vmo, offset, length);
}

/// `[offset, offset + length)` 中第一段脏区间。
pub fn first_dirty(vmo: &Vmo, offset: u64, length: u64) -> Option<(u64, u64)> {
    shared()
        .pager
        .op(pager_op::OP_QUERY_DIRTY, vmo, offset, length)
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
                u64::from(request.observed) * native_abi::PAGE_SIZE as u64,
                &mut aux,
            );
        }
    }
}
