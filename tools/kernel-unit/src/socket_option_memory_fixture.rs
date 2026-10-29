//! 实际 NET policy 与 socket memory owner；轻量 facade 只转发实际 owner，不复制 admission 算法。
mod inet {
    // 本fixture只执行未发布NET policy；不会安装此类型，真实transport在network_endpoint_fixture覆盖。
    pub(crate) struct NetworkStackOwner;
}
#[path = "../../../kernel/src/net/net_namespace.rs"]
mod net_namespace;
pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
#[derive(Debug)]
pub(crate) enum SocketError {
    NoMemory,
}
#[path = "../../../kernel/src/net/control_memory.rs"]
mod control_memory;
pub(crate) use control_memory::{ControlMemoryCharge, SocketMemory};
pub(crate) struct Socket {
    pub(crate) memory: alloc::sync::Arc<SocketMemory>,
}
impl Socket {
    pub(crate) fn reserve_control_memory(&self, bytes: usize) -> Option<ControlMemoryCharge> {
        self.memory.reserve(bytes)
    }
}
pub(crate) fn endpoint() -> Socket {
    crate::namespace::boot();
    initialize_net().unwrap();
    Socket {
        memory: SocketMemory::try_new(initial_net()).unwrap(),
    }
}
