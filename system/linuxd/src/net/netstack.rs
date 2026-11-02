//! netstack 连接：网络域的创建与 socket 就绪事件的分派。
//!
//! 1. 经根服务的服务目录连接 netstack 的当前实例；每个网络域在 netstack 中有一个协议实例，经域 channel 访问，
//!    域对象释放时 channel 关闭，netstack 退休该实例；
//! 2. 每个 AF_INET/AF_PACKET socket 带一个 netstack 置位的就绪 Event：分派线程收到后唤醒该 socket 的本地
//!    wait source，等待者被唤醒后经 RPC 重新查询电平状态；
//! 3. 初始网络域对应 netstack 的默认网络，它随 netstack 存续：linuxd 重启后重新连上，接口配置不丢失；
//! 4. netstack 终止时连接 `PEER_CLOSED`：监视线程重新连接（根服务在重启完成后转发），为每个存活网络域重建
//!    协议实例。旧 socket 的 channel 已随旧实例关闭，之后的操作以 `ENETDOWN` 失败；重建的默认网络由
//!    netstack 的接口配置 owner 重新配置，其他网络域与新建网络域一样从未配置状态开始。

use alloc::sync::Arc;

use crate::sync::{Mutex, Once};
use device_protocol::{ActivityBinding, EventHub};
use native_runtime::Event;
use native_runtime::native_abi::{self, signals};
use network_protocol::{NetworkClient, RemoteNamespace, SocketError};

use super::NetNamespace;
use crate::ipc::PipeEnd;

// OWNER: socket 就绪 Event 的唯一分派线程；`init` 启动一次，缺失时 socket 创建以 NetworkDown 失败。
static EVENTS: Once<Arc<EventHub>> = Once::new();
// OWNER: 到 netstack 当前实例的连接；监视线程在实例终止后换成新连接。缺失时网络域无法创建。
static CONTROL: Mutex<Option<Arc<NetworkClient>>> = Mutex::new(None);

fn connect() -> Result<NetworkClient, native_abi::Status> {
    crate::handles::services()
        .connect(service_directory::NETWORK)
        .map(NetworkClient::new)
}

fn control() -> Result<Arc<NetworkClient>, SocketError> {
    CONTROL.lock().clone().ok_or(SocketError::NetworkDown)
}

/// 启动就绪事件分派，连接 netstack 并把初始网络域接到它的默认网络（全部网卡、回环启用），
/// 然后启动重连监视线程。
///
/// # Panics
///
/// 分派线程、连接、初始域或监视线程创建失败：没有初始域就没有任何网络。
pub(crate) fn init() {
    EVENTS.call_once(|| EventHub::start().expect("network event hub"));
    let client = Arc::new(connect().expect("netstack connection"));
    let initial = client
        .create_namespace(true)
        .expect("initial network namespace");
    super::net_namespace::initial_net_ref().install_protocol(initial);
    *CONTROL.lock() = Some(client.clone());
    let thread = native_runtime::thread::spawn(b"netstack-monitor", move || monitor(client))
        .expect("netstack monitor thread");
    drop(thread);
}

/// 等待当前连接断开，重连并为全部存活网络域重建协议实例；重连失败（根服务判定崩溃循环后关机）时结束。
fn monitor(mut client: Arc<NetworkClient>) {
    loop {
        let _ = client
            .channel()
            .wait_one(signals::PEER_CLOSED, native_abi::DEADLINE_INFINITE);
        CONTROL.lock().take();
        warn!("netstack connection closed; reconnecting");
        let Ok(next) = connect() else {
            error!("netstack reconnection failed");
            return;
        };
        client = Arc::new(next);
        rebuild(&client);
        *CONTROL.lock() = Some(client.clone());
    }
}

fn rebuild(client: &NetworkClient) {
    let initial = super::net_namespace::initial_net_ref();
    match client.create_namespace(true) {
        Ok(protocol) => initial.install_protocol(protocol),
        Err(status) => error!("initial network namespace rebuild failed: {status}"),
    }
    for namespace in super::net_namespace::live_namespaces() {
        match client.create_namespace(false) {
            Ok(protocol) => namespace.install_protocol(protocol),
            Err(status) => error!("network namespace rebuild failed: {status}"),
        }
    }
}

/// 为新网络域创建 netstack 协议实例：只有 DOWN 且无地址的回环，没有旧域的设备或配置。
///
/// # Errors
///
/// netstack 资源不足、已终止或正在重启。
pub(super) fn create_namespace() -> Result<RemoteNamespace, SocketError> {
    control()?.create_namespace(false).map_err(|status| {
        if status == native_abi::ERR_NO_MEMORY {
            SocketError::NoMemory
        } else {
            SocketError::NetworkDown
        }
    })
}

/// 域的当前协议实例；装配前或 netstack 重启期间报告网络不可达。
pub(super) fn protocol(scope: &NetNamespace) -> Result<Arc<RemoteNamespace>, SocketError> {
    scope.protocol().ok_or(SocketError::NetworkUnreachable)
}

/// 把 netstack 的就绪 Event 接到本地 notification：Event 置位时唤醒 `notify` 的读端等待者。
///
/// # Errors
///
/// 分派登记或闭包分配失败返回 `NoMemory`。
pub(super) fn bind_readiness(
    event: Event,
    notify: &Arc<PipeEnd>,
) -> Result<ActivityBinding, SocketError> {
    let hub = EVENTS.get().ok_or(SocketError::NetworkDown)?;
    let binding = hub.activity(event).map_err(|_| SocketError::NoMemory)?;
    let notify = notify.clone();
    let activity: device_model::Activity =
        Arc::try_new(move || notify.signal_readiness()).map_err(|_| SocketError::NoMemory)?;
    binding.bind(activity);
    Ok(binding)
}

/// 摘除 socket 的就绪登记（最后一次文件关闭）。
pub(super) fn retire_readiness(binding: &ActivityBinding) {
    if let Some(hub) = EVENTS.get() {
        hub.retire(binding);
    }
}
