//! 域与 socket channel 的分派：全部 channel 在一个 port 上等待，若干工作线程处理就绪的 channel。
//!
//! 1. 登记 channel 时以其 key 对 port 发出一次性等待（可读或对端关闭）；
//! 2. 工作线程取到 packet 后读空该 channel 的请求逐条回复，再重新发出等待——同一 channel 同时只由一个
//!    线程处理，请求按到达顺序串行，不同 channel 并行；
//! 3. 对端关闭（使用者关闭 socket 或退休域）时摘除登记并通知被服务对象。

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status, signals};
use native_runtime::sync::Once;
use native_runtime::{Channel, Handle, Port};
use network_protocol::{Reader, ServedSocket, Writer};

use crate::server::NamespaceServer;
use crate::sync::Mutex;

/// 分派工作线程数：socket 操作不阻塞，少量线程即可让不同 socket 的请求并行。
const WORKERS: usize = 4;

/// 一条 channel 服务的对象。
pub(crate) enum Served {
    Namespace(NamespaceServer),
    Socket(ServedSocket),
}

impl Served {
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
}

struct Dispatcher {
    port: Port,
    // OWNER: key → 登记项的唯一索引；摘除后迟到的 packet 查不到登记项而被忽略。
    entries: Mutex<fallible_tree::FallibleMap<u64, Arc<Entry>>>,
    next_key: core::sync::atomic::AtomicU64,
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
    });
    for _ in 0..WORKERS {
        let thread = native_runtime::thread::spawn(b"netd-dispatch", run)?;
        // 工作线程与服务同寿命。
        drop(thread);
    }
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
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let entry = Arc::try_new(Entry { channel, served }).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let Ok(prepared) = fallible_tree::FallibleMap::try_prepare(key, entry.clone()) else {
        entry.served.close();
        return Err(native_abi::ERR_NO_MEMORY);
    };
    dispatcher.entries.lock().commit_vacant(prepared);
    if let Err(status) = arm(dispatcher, &entry, key) {
        retire(dispatcher, key);
        return Err(status);
    }
    Ok(())
}

fn arm(dispatcher: &Dispatcher, entry: &Entry, key: u64) -> Result<(), Status> {
    entry.channel.wait_async(
        &dispatcher.port,
        key,
        signals::READABLE | signals::PEER_CLOSED,
    )
}

fn retire(dispatcher: &Dispatcher, key: u64) {
    let removed = dispatcher.entries.lock().take_entry(&key);
    if let Some(entry) = removed {
        entry.value().served.close();
    }
}

fn run() {
    let dispatcher = dispatcher();
    loop {
        let Ok(packet) = dispatcher.port.wait(DEADLINE_INFINITE) else {
            continue;
        };
        let Some(entry) = dispatcher.entries.lock().get(&packet.key).cloned() else {
            continue;
        };
        let open = loop {
            match entry.channel.read() {
                Ok(message) => {
                    let handler = |ordinal: u32,
                                   request: &mut Reader<'_>,
                                   handles: Vec<Handle>,
                                   reply: &mut Writer| {
                        entry.served.dispatch(ordinal, request, handles, reply)
                    };
                    if service_rpc::respond(&entry.channel, message, &handler).is_err() {
                        break false;
                    }
                }
                Err(native_abi::ERR_SHOULD_WAIT) => break true,
                Err(_) => break false,
            }
        };
        if !open || arm(dispatcher, &entry, packet.key).is_err() {
            retire(dispatcher, packet.key);
        }
    }
}
