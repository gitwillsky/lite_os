//! netd 连接：网络域的创建与 socket 就绪事件的分派。
//!
//! 1. 每个网络域在 netd 中有一个协议实例，经域 channel 访问；域对象释放时 channel 关闭，netd 退休该实例；
//! 2. 每个 AF_INET/AF_PACKET socket 带一个 netd 置位的就绪 Event：分派线程收到后唤醒该 socket 的本地
//!    wait source，等待者被唤醒后经 RPC 重新查询电平状态。

use alloc::sync::Arc;

use crate::sync::Once;
use device_protocol::{ActivityBinding, EventHub};
use native_runtime::Event;
use network_protocol::{RemoteNamespace, SocketError};

use super::NetNamespace;
use crate::ipc::PipeEnd;

// OWNER: socket 就绪 Event 的唯一分派线程；`init` 启动一次，缺失时 socket 创建以 NetworkDown 失败。
static EVENTS: Once<Arc<EventHub>> = Once::new();

/// 启动就绪事件分派，并为初始网络域创建 netd 协议实例（接入全部网卡、启用回环）。
///
/// # Panics
///
/// 分派线程或初始域创建失败：没有初始域就没有任何网络。
pub(crate) fn init() {
    EVENTS.call_once(|| EventHub::start().expect("network event hub"));
    let initial = crate::handles::network()
        .create_namespace(true)
        .expect("initial network namespace");
    super::net_namespace::initial_net_ref().install_protocol(initial);
}

/// 为新网络域创建 netd 协议实例：只有 DOWN 且无地址的回环，没有旧域的设备或配置。
///
/// # Errors
///
/// netd 资源不足或已终止。
pub(super) fn create_namespace() -> Result<RemoteNamespace, SocketError> {
    crate::handles::network()
        .create_namespace(false)
        .map_err(|status| {
            if status == native_runtime::native_abi::ERR_NO_MEMORY {
                SocketError::NoMemory
            } else {
                SocketError::NetworkDown
            }
        })
}

/// 域的协议实例；装配前（不可能在 Task 可见之后发生）报告网络不可达。
pub(super) fn protocol(scope: &NetNamespace) -> Result<&RemoteNamespace, SocketError> {
    scope.protocol().ok_or(SocketError::NetworkUnreachable)
}

/// 把 netd 的就绪 Event 接到本地 notification：Event 置位时唤醒 `notify` 的读端等待者。
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
