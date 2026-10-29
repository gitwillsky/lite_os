#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 完整实际 AF_UNIX endpoint 的内存/queue owner 测量，producer 只提供原生调用输入。
extern crate alloc;
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;
mod cost_policy;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../kernel-unit/src/unix_rights_fixture/file.rs"]
mod file;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../../kernel-unit/src/pipe_fixture.rs"]
mod ipc;
#[path = "../../kernel-unit/src/namespace_fixture.rs"]
mod namespace;
#[path = "../../kernel-unit/src/unix_endpoint_fixture.rs"]
mod net;

use alloc::sync::Arc;
use std::{hint::black_box, time::Instant};
fn endpoint() -> (Arc<net::UnixSocket>, Arc<file::OpenFileDescription>) {
    let endpoint = net::UnixSocket::new(
        net::SocketType::SeqPacket,
        ipc::Pipe::notification_pair().unwrap(),
        id::next_runtime_object_id(),
        net::boot_net(),
    )
    .unwrap();
    let ofd = Arc::new(file::OpenFileDescription {
        kind: file::OpenFileKind::Socket(Arc::new(net::Socket {
            endpoint: endpoint.clone(),
        })),
    });
    net::bind_file(&endpoint, &ofd);
    (endpoint, ofd)
}
fn measure(name: &str, iterations: u64, gated: bool, mut operation: impl FnMut()) {
    for _ in 0..iterations {
        operation();
    }
    let mut samples = [0.0f64; cost_policy::SAMPLES];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        *sample = start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    println!("{name}: {median:.2} ns/op");
    if gated {
        assert!(
            median <= cost_policy::MAX_NANOSECONDS_PER_OPERATION,
            "{name}: hot-path policy exceeded"
        );
    }
}
fn main() {
    namespace::boot();
    let mut pid = namespace::PreparedPid::try_new(
        72414,
        namespace::initial_pid(),
        &[],
        &namespace::root_authority(),
    )
    .unwrap();
    pid.publish().unwrap();
    let _registration = pid.take_registration();
    let credentials = net::UnixCredentials {
        pid: pid.identity(),
        uid: 1000,
        gid: 2000,
    };
    let (sender, _sender_file) = endpoint();
    let (receiver, _receiver_file) = endpoint();
    net::UnixSocket::pair(
        &sender,
        &receiver,
        net::UnixConnectionData::Packet,
        credentials.clone(),
    )
    .unwrap();
    let input = net::UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    measure(
        "actual UNIX packet length ledger",
        cost_policy::ITERATIONS,
        true,
        || {
            let _ = black_box(sender.validate_packet_length(black_box(100_000)));
        },
    );
    measure(
        "actual UNIX packet memory and peer readiness",
        cost_policy::ITERATIONS,
        true,
        || {
            black_box(sender.poll_state());
        },
    );
    // Native 每条 message 分配实际 node/payload，冷成本独立报告；不放宽 hot lock/projection 的门禁。
    let mut bytes = [0u8; 32];
    measure(
        "actual UNIX packet allocation/publication/consume/refund",
        20_000,
        false,
        || {
            assert!(matches!(
                sender.send_to_with_ancillary(black_box(b"packet"), None, &mut None, &input),
                Ok(6)
            ));
            let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
            black_box(net::receive(&receiver, &mut output, false, false, true).unwrap());
        },
    );
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../../kernel-unit/src/sync.rs"]
    pub(crate) mod sync;
}
