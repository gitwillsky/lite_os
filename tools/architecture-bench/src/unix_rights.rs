#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 实际 SCM graph/OFD probe 的 CPU 测量；host endpoint façade 是 producer seam。
extern crate alloc;
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;
mod cost_policy;
#[path = "../../../kernel/src/execution/task/model/message_identity.rs"]
mod message_identity;
// 实际 scheduler cadence 为 benchmark 边界；policy/锁/deadline 运算全部来自 production。

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
#[path = "../../kernel-unit/src/unix_rights_fixture/mod.rs"]
mod net;
#[path = "../../../kernel/src/net/timeouts.rs"]
mod socket_timeouts;

use alloc::{sync::Arc, vec};
use net::{UnixPassedFile, UnixRights, UnixSocket};
use std::{hint::black_box, time::Instant};
fn endpoint() -> (Arc<UnixSocket>, Arc<file::OpenFileDescription>) {
    let socket = UnixSocket::new();
    let file = Arc::new(file::OpenFileDescription {
        kind: file::OpenFileKind::Socket(Arc::new(net::Socket {
            endpoint: socket.clone(),
        })),
    });
    socket.bind_file(&file);
    (socket, file)
}
fn rights(file: &Arc<file::OpenFileDescription>, uid: u32, limit: Option<usize>) -> UnixRights {
    UnixRights::new(
        vec![file.clone()],
        uid,
        limit.map(|limit| -> Arc<dyn net::UnixFileLimit> { net::ProducedLimit::new(limit) }),
    )
    .unwrap()
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
    let view = namespace::initial_pid();
    let mut prepared_pid =
        namespace::PreparedPid::try_new(72200, view.clone(), &[], &namespace::root_authority())
            .unwrap();
    prepared_pid.publish().unwrap();
    let registration = prepared_pid.take_registration();
    let number = prepared_pid.identity().number_in(&view);
    measure(
        "native PID namespace observer lookup",
        cost_policy::ITERATIONS,
        true,
        || {
            let _ = black_box(view.lookup_identity(black_box(number)));
        },
    );
    let authority = namespace::root_authority();
    let identity =
        message_identity::MessageIdentity::new(authority, [1001, 1002, 1003], [2001, 2002, 2003]);
    let pid_owner = view.user_namespace();
    measure(
        "actual native message credential claim validation",
        cost_policy::ITERATIONS,
        true,
        || {
            let _ = black_box(identity.validate_claim(
                black_box(1001),
                black_box(2001),
                black_box(true),
                &pid_owner,
            ));
        },
    );
    drop(registration);

    let timeouts = socket_timeouts::SocketTimeouts::new();
    timeouts.set(
        socket_timeouts::SocketIoDirection::Receive,
        socket_timeouts::SocketTimeout::finite(50_000_000),
    );
    measure(
        "socket actual passive timeout policy freeze",
        cost_policy::ITERATIONS,
        true,
        || {
            let _ = black_box(timeouts.snapshot());
        },
    );
    measure(
        "socket actual timeout policy snapshot/deadline",
        cost_policy::ITERATIONS,
        true,
        || {
            let _ = black_box(
                timeouts
                    .get(black_box(socket_timeouts::SocketIoDirection::Receive))
                    .deadline(black_box(123_456)),
            );
        },
    );
    let (credential_target, _credential_file) = endpoint();
    let credential_channel = UnixSocket::stream_channel(&credential_target);
    let sender = net::UnixCredentials {
        pid: prepared_pid.identity(),
        uid: 1001,
        gid: 2001,
    };
    credential_channel
        .write_credentials(b"c", &mut None, Some(&sender))
        .unwrap();
    let mut credential_bytes = [0u8; 1];
    measure(
        "actual stream credential span peek",
        cost_policy::ITERATIONS,
        true,
        || {
            let mut output = ipc::ReceiveBuffer::from_slice(&mut credential_bytes);
            black_box(
                credential_channel
                    .read_credentials(&mut output, false, true, true)
                    .unwrap(),
            );
        },
    );
    measure(
        "actual stream chunk admission and adjacent credential consume",
        cost_policy::ITERATIONS,
        true,
        || {
            black_box(
                credential_channel
                    .write_credentials(b"c", &mut None, Some(&sender))
                    .unwrap(),
            );
            let mut output = ipc::ReceiveBuffer::from_slice(&mut credential_bytes);
            black_box(
                credential_channel
                    .read_credentials(&mut output, false, false, true)
                    .unwrap(),
            );
        },
    );
    measure(
        "cold stream construction and first credential frame admission",
        128,
        false,
        || {
            let channel = UnixSocket::stream_channel(&credential_target);
            black_box(
                channel
                    .write_credentials(b"c", &mut None, Some(&sender))
                    .unwrap(),
            );
        },
    );
    let (target, file) = endpoint();
    let mut first = rights(&file, 8201, Some(0));
    UnixSocket::prepare(&mut first, &target).unwrap();
    UnixSocket::publish_prepared(&mut first, &target);
    let mut rejected = rights(&file, 8201, Some(0));
    UnixSocket::prepare(&mut rejected, &target).unwrap();
    measure(
        "SCM exact UID quota rejection without allocation",
        cost_policy::ITERATIONS,
        true,
        || {
            assert!(black_box(UnixSocket::quota_rejected(
                &mut rejected,
                &target
            )));
        },
    );
    drop(first);
    measure(
        "SCM actual cold preparation/publication/retirement",
        20_000,
        false,
        || {
            let mut batch = rights(&file, 8202, None);
            UnixSocket::prepare(&mut batch, &target).unwrap();
            UnixSocket::publish_prepared(&mut batch, &target);
            black_box(batch.into_files());
        },
    );
    let channel = UnixSocket::stream_channel(&target);
    channel.write(b"hello", &mut None).unwrap();
    let mut bytes = [0u8; 5];
    measure(
        "AF_UNIX actual chunk FIFO MSG_PEEK without controls",
        cost_policy::ITERATIONS,
        true,
        || {
            let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
            black_box(channel.peek(&mut output, false).unwrap());
        },
    );
    for count in [1, 253] {
        let channel = UnixSocket::stream_channel(&target);
        let passed = (0..count)
            .map(|_| -> Arc<dyn UnixPassedFile> { file.clone() })
            .collect();
        let mut control = Some(UnixRights::new(passed, 8204, None).unwrap());
        channel.write(b"hello", &mut control).unwrap();
        // Native SCM_PEEK clone 必须分配每次输出的 capability storage；记录其冷路径成本。
        measure(
            &format!("AF_UNIX MSG_PEEK actual allocation/clone/fput, {count} rights"),
            20_000,
            false,
            || {
                let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
                black_box(channel.peek(&mut output, true).unwrap());
            },
        );
    }
    for count in [16, 256] {
        let mut roots = Vec::with_capacity(count);
        for _ in 0..count {
            let (socket, file) = endpoint();
            socket.enqueue(rights(&file, 8203, None));
            roots.push((socket, file));
        }
        measure(
            &format!("SCM actual rooted Tarjan pass {count} nodes"),
            2_000,
            false,
            || {
                assert!(black_box(UnixSocket::collect(8203, 0)).is_err());
            },
        );
        for (socket, _) in roots {
            socket.release_queue();
        }
    }
}

#[path = "."]
pub(crate) mod execution {
    pub(crate) mod timer {
        pub(crate) const SCHEDULER_TICK_NS: u64 = 10_000_000;
    }
    #[path = "../../kernel-unit/src/sync.rs"]
    pub(crate) mod sync;
}
