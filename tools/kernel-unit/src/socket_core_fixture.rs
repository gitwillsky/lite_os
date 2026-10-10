//! 实际 core tunable owner；本 fixture 不执行 packet-layout-dependent minimum，实际 Unix owner 回归覆盖它。
mod inet {
    // 未安装protocol的显式类型边界；实际传输/析构由network_endpoint_fixture载入production owner覆盖。
    pub(crate) struct NetworkStackOwner;
}
#[path = "../../../kernel/src/socket/core_buffer_limit.rs"]
mod selector;
pub(crate) use selector::CoreBufferLimit;
#[derive(Clone, Copy)]
pub(crate) enum SocketIoDirection {
    Send,
    Receive,
}
mod unix {
    pub(super) fn minimum_buffer(_: super::SocketIoDirection) -> usize {
        unreachable!("packet layout boundary: covered by actual unix_endpoint_fixture")
    }
}
#[path = "../../../kernel/src/socket/net_namespace.rs"]
mod net_namespace;
#[path = "../../../kernel/src/socket/core_buffers.rs"]
mod owner;
pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
pub(crate) fn boot_net() -> alloc::sync::Arc<NetNamespace> {
    crate::namespace::boot();
    initialize_net().unwrap();
    initial_net()
}

/// Native namespace creation使用实际network endpoint fixture；本fixture的protocol是显式未装配边界。
pub(crate) fn create_net_namespace(
    _: alloc::sync::Arc<crate::namespace::UserNamespace>,
    _: u32,
) -> Result<alloc::sync::Arc<NetNamespace>, crate::namespace::NamespaceError> {
    unreachable!(
        "full native protocol constructor is covered by actual network endpoint fixture and guest"
    )
}
