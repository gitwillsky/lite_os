//! 实际 NET policy 与 socket memory owner；轻量 facade 只转发实际 owner，不复制 admission 算法。
mod inet {
    // 本fixture只执行未发布NET policy；不会安装此类型，真实 transport 在 netd-unit 与 guest 中覆盖。
    pub(crate) struct NetworkStackOwner;
}
#[path = "../../../system/linuxd/src/net/net_namespace.rs"]
mod net_namespace;
pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
pub(crate) use network_wire::SocketError;
#[path = "../../../system/linuxd/src/net/control_memory.rs"]
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

mod netd {
    pub(super) fn create_namespace() -> Result<network_protocol::RemoteNamespace, super::SocketError> {
        Ok(network_protocol::RemoteNamespace)
    }
}
