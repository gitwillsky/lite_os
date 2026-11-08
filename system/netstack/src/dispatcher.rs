//! 域与 socket channel 的分派：全部 channel 在一个 port 上等待，若干工作线程处理就绪的 channel。
//!
//! 1. 登记 channel 时以其 key 对 port 发出一次性等待（可读或对端关闭）；
//! 2. 工作线程取到 packet 后串行处理该 socket 的控制消息与共享 SQ，各来源分别重武装；
//!    每轮有界，防止热 socket 独占分派；跨控制/数据来源不承诺并发调用的顺序；
//! 3. 对端关闭（使用者关闭 socket 或退休域）时摘除登记并通知被服务对象。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status, signals};
use native_runtime::sync::Once;
use native_runtime::{Channel, Handle, Port};
use network_protocol::{Reader, ServedSocket, SocketQueue, Writer};

use crate::server::NamespaceServer;
use crate::sync::Mutex;

/// 分派工作线程数：socket 操作不阻塞，少量线程即可让不同 socket 的请求并行。
const WORKERS: usize = 4;
const IDLE_NS: i64 = 5_000_000_000;
struct Workers {
    total: usize,
    idle: usize,
}
struct Sources {
    // FLAG: 两个来源各自的一次性等待是否尚未消费；共用一个位会重复武装或漏唤醒。
    channel_armed: bool,
    queue_armed: bool,
    retired: bool,
}

/// 一条 channel 服务的对象。
pub(crate) enum Served {
    Namespace(NamespaceServer),
    Socket(ServedSocket),
}

impl Served {
    fn closes_connection(&self, ordinal: u32) -> bool {
        matches!(self, Self::Socket(socket) if socket.closes_connection(ordinal))
    }
    fn dispatch(
        &self,
        ordinal: u32,
        request: &mut Reader<'_>,
        handles: Vec<Handle>,
        reply: &mut Writer,
    ) -> Result<Vec<Handle>, Status> {
        match self {
            Self::Namespace(namespace) => {
                network_protocol::dispatch_namespace(namespace, ordinal, request, handles, reply)
            }
            Self::Socket(socket) => socket.dispatch(ordinal, request, handles, reply),
        }
    }

    fn close(&self) {
        if let Self::Socket(socket) = self {
            socket.close();
        }
    }
}

struct Entry {
    channel: Channel,
    served: Served,
    queue: Option<SocketQueue>,
    // OWNER: 同一 socket 的控制与数据操作串行，退休与重武装也在这一临界区中决定。
    sources: Mutex<Sources>,
}

struct Dispatcher {
    port: Port,
    // OWNER: key → 登记项的唯一索引；摘除后迟到的 packet 查不到登记项而被忽略。
    entries: Mutex<fallible_tree::FallibleMap<u64, Arc<Entry>>>,
    next_key: core::sync::atomic::AtomicU64,
    // OWNER: 弹性工作线程准入；始终保留一个空闲读者，繁忙时才占用额外线程栈。
    workers: Mutex<Workers>,
}

// OWNER: 进程唯一分派器；`start` 安装一次，缺失时任何 channel 都无法登记。
static DISPATCHER: Once<Dispatcher> = Once::new();

/// 创建 port 与工作线程。
///
/// # Errors
///
/// port 或线程创建失败。
pub(crate) fn start() -> Result<(), Status> {
    let port = Port::create()?;
    DISPATCHER.call_once(|| Dispatcher {
        port,
        entries: Mutex::new(fallible_tree::FallibleMap::new()),
        next_key: core::sync::atomic::AtomicU64::new(1),
        workers: Mutex::new(Workers { total: 1, idle: 1 }),
    });
    drop(native_runtime::thread::spawn(b"netstack-dispatch", run)?);
    Ok(())
}

fn dispatcher() -> &'static Dispatcher {
    DISPATCHER
        .get()
        .expect("dispatcher started before serving channels")
}

/// 登记 `channel`，以 `served` 处理它的请求。
///
/// # Errors
///
/// 分配或等待登记失败；失败时 `served` 已被通知关闭或随登记项释放（socket 的 Drop 关闭协议状态）。
pub(crate) fn serve(channel: Channel, served: Served) -> Result<(), Status> {
    let dispatcher = dispatcher();
    let key = dispatcher
        .next_key
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |key| {
            key.checked_add(1).filter(|next| *next <= u64::MAX / 2)
        })
        .map_err(|_| native_abi::ERR_NO_RESOURCES)?;
    let queue = match &served {
        Served::Socket(_) => Some(SocketQueue::create()?),
        Served::Namespace(_) => None,
    };
    let entry = Arc::try_new(Entry {
        channel,
        served,
        queue,
        sources: Mutex::new(Sources {
            channel_armed: false,
            queue_armed: false,
            retired: false,
        }),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let Ok(prepared) = fallible_tree::FallibleMap::try_prepare(key, entry.clone()) else {
        entry.served.close();
        return Err(native_abi::ERR_NO_MEMORY);
    };
    dispatcher.entries.lock().commit_vacant(prepared);
    let armed = {
        let mut sources = entry.sources.lock();
        let result = arm(dispatcher, &entry, key, &mut sources);
        sources.retired = result.is_err();
        result
    };
    if let Err(status) = armed {
        retire(dispatcher, key);
        return Err(status);
    }
    Ok(())
}

fn arm(
    dispatcher: &Dispatcher,
    entry: &Entry,
    key: u64,
    sources: &mut Sources,
) -> Result<(), Status> {
    if !sources.channel_armed {
        entry.channel.wait_async(
            &dispatcher.port,
            key * 2,
            signals::READABLE | signals::PEER_CLOSED,
        )?;
        sources.channel_armed = true;
    }
    if !sources.queue_armed
        && let Some(queue) = &entry.queue
    {
        queue
            .requests()
            .wait_async(&dispatcher.port, key * 2 + 1, signals::SIGNALED)?;
        sources.queue_armed = true;
    }
    Ok(())
}

fn retire(dispatcher: &Dispatcher, key: u64) {
    let removed = dispatcher.entries.lock().take_entry(&key);
    if let Some(entry) = removed {
        if let Some(queue) = &entry.value().queue {
            queue.close();
        }
        entry.value().served.close();
    }
}

/// 每次最多处理有限控制消息和一批 SQ，避免一个繁忙 socket 独占 worker。
fn dispatch(entry: &Entry) -> bool {
    for _ in 0..16 {
        match entry.channel.read() {
            Ok(message) => {
                // respond 同步执行但 Handler 要求 Sync；此栈内标记只表示本条回复后的退休后果。
                let retire_after_reply = AtomicBool::new(false);
                let handler = |ordinal: u32,
                               request: &mut Reader<'_>,
                               handles: Vec<Handle>,
                               reply: &mut Writer| {
                    if let Some(queue) = &entry.queue
                        && let Some(result) = queue.negotiate(ordinal, reply)
                    {
                        return result;
                    }
                    let handles = entry.served.dispatch(ordinal, request, handles, reply)?;
                    retire_after_reply
                        .store(entry.served.closes_connection(ordinal), Ordering::Relaxed);
                    Ok(handles)
                };
                if service_rpc::respond(&entry.channel, message, &handler).is_err()
                    || retire_after_reply.load(Ordering::Relaxed)
                {
                    return false;
                }
            }
            Err(native_abi::ERR_SHOULD_WAIT) => break,
            Err(_) => return false,
        }
    }
    if let (Some(queue), Served::Socket(socket)) = (&entry.queue, &entry.served) {
        if queue.requests().signal(signals::SIGNALED, 0).is_err() {
            return false;
        }
        match queue.drain(socket) {
            Ok(true) => {
                if queue.requests().signal(0, signals::SIGNALED).is_err() {
                    return false;
                }
            }
            Ok(false) => (),
            Err(_) => return false,
        }
    }
    true
}
fn run() {
    let dispatcher = dispatcher();
    loop {
        let deadline = if dispatcher.workers.lock().idle <= 1 {
            DEADLINE_INFINITE
        } else {
            native_runtime::system::deadline_after(IDLE_NS)
        };
        let packet = match dispatcher.port.wait(deadline) {
            Ok(packet) => packet,
            Err(native_abi::ERR_TIMED_OUT) => {
                let mut workers = dispatcher.workers.lock();
                if workers.idle > 1 {
                    workers.total -= 1;
                    workers.idle -= 1;
                    return;
                }
                continue;
            }
            Err(_) => continue,
        };
        let grow = {
            let mut workers = dispatcher.workers.lock();
            workers.idle -= 1;
            let grow = workers.idle == 0 && workers.total < WORKERS;
            if grow {
                workers.total += 1;
                workers.idle += 1;
            }
            grow
        };
        if grow && native_runtime::thread::spawn(b"netstack-dispatch", run).is_err() {
            let mut workers = dispatcher.workers.lock();
            workers.total -= 1;
            workers.idle -= 1;
        }
        let key = packet.key / 2;
        let entry = dispatcher.entries.lock().get(&key).cloned();
        if let Some(entry) = entry {
            let mut sources = entry.sources.lock();
            if !sources.retired {
                if packet.key & 1 == 0 {
                    sources.channel_armed = false;
                } else {
                    sources.queue_armed = false;
                }
                if !dispatch(&entry) || arm(dispatcher, &entry, key, &mut sources).is_err() {
                    sources.retired = true;
                    retire(dispatcher, key);
                }
            }
        }
        dispatcher.workers.lock().idle += 1;
    }
}
