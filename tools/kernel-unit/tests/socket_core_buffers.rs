#![feature(allocator_ext)]
#![allow(dead_code)]
//! 调优写入真实 global owner；独立 binary 避免其他 endpoint 回归并行改变默认预算。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/unix_rights_fixture/file.rs"]
mod file;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/unix_endpoint_fixture.rs"]
mod net;

use alloc::sync::Arc;
use net::{CoreBufferLimit, SocketIoDirection, SocketType, UnixSocket};

struct Restore([u32; 4]);
impl Drop for Restore {
    fn drop(&mut self) {
        for (resource, value) in CoreBufferLimit::ALL.into_iter().zip(self.0) {
            resource.set(value).unwrap();
        }
    }
}
fn endpoint() -> Arc<UnixSocket> {
    UnixSocket::new(
        SocketType::Datagram,
        ipc::Pipe::notification_pair().unwrap(),
        id::next_runtime_object_id(),
        net::boot_net(),
    )
    .unwrap()
}
#[test]
fn actual_global_tuning_changes_future_defaults_and_option_clamps_only() {
    use CoreBufferLimit::{ReceiveDefault, ReceiveMaximum, SendDefault, SendMaximum};
    use SocketIoDirection::{Receive, Send};
    namespace::boot();
    let _restore = Restore(CoreBufferLimit::ALL.map(|resource| resource.value()));
    assert_eq!(SendDefault.value(), 212_992);
    assert_eq!(ReceiveDefault.value(), 212_992);
    assert_eq!(SendMaximum.value(), 4 << 20);
    assert_eq!(ReceiveMaximum.value(), 4 << 20);
    let original = endpoint();
    SendDefault.set(32_768).unwrap();
    ReceiveDefault.set(16_384).unwrap();
    SendMaximum.set(65_536).unwrap();
    ReceiveMaximum.set(32_768).unwrap();
    let fresh = endpoint();
    assert_eq!(original.buffer_limit(Send), 212_992);
    assert_eq!(original.buffer_limit(Receive), 212_992);
    assert_eq!(fresh.buffer_limit(Send), 32_768);
    assert_eq!(fresh.buffer_limit(Receive), 16_384);
    original.set_buffer_limit(Send, -1, false);
    original.set_buffer_limit(Receive, i32::MAX, false);
    assert_eq!(original.buffer_limit(Send), 131_072);
    assert_eq!(original.buffer_limit(Receive), 65_536);
    SendMaximum.set(8192).unwrap();
    assert_eq!(original.buffer_limit(Send), 131_072);
    original.set_buffer_limit(Send, -1, false);
    assert_eq!(original.buffer_limit(Send), 16_384);
    original.set_buffer_limit(Send, 65_536, true);
    original.set_buffer_limit(Receive, 65_536, true);
    assert_eq!(original.buffer_limit(Send), 131_072);
    assert_eq!(original.buffer_limit(Receive), 131_072);
    original.set_buffer_limit(Send, -1, true);
    original.set_buffer_limit(Receive, -1, true);
    assert_eq!(original.buffer_limit(Send), SendMaximum.minimum() as i32);
    assert_eq!(
        original.buffer_limit(Receive),
        ReceiveMaximum.minimum() as i32
    );
    original.set_buffer_limit(Send, 0, false);
    original.set_buffer_limit(Receive, 0, false);
    assert_eq!(original.buffer_limit(Send), SendMaximum.minimum() as i32);
    assert_eq!(
        original.buffer_limit(Receive),
        ReceiveMaximum.minimum() as i32
    );
    assert_eq!(SendMaximum.minimum(), ReceiveMaximum.minimum() * 2);
    // 最大值及 default 独立；default 不经 setsockopt 倍增或当前 max 二次 clamp。
    SendDefault.set(65_536).unwrap();
    let larger_default = endpoint();
    assert_eq!(larger_default.buffer_limit(Send), 65_536);
    SendMaximum.set(i32::MAX as u32).unwrap();
    original.set_buffer_limit(Send, -1, false);
    assert_eq!(original.buffer_limit(Send), i32::MAX - 1);
    original.set_buffer_limit(Receive, i32::MAX, true);
    assert_eq!(original.buffer_limit(Receive), i32::MAX - 1);
    for resource in CoreBufferLimit::ALL {
        let before = resource.value();
        for invalid in [0, resource.minimum() - 1, (i32::MAX as u32) + 1, u32::MAX] {
            assert!(matches!(
                resource.set(invalid),
                Err(namespace::NamespaceError::Invalid)
            ));
            assert_eq!(resource.value(), before);
        }
    }
    net::close_description(&original);
    net::close_description(&fresh);
    net::close_description(&larger_default);
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
}
