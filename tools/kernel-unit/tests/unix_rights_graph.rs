#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 SCM graph：无分配 publication、OOM 回滚、重复边与 SCC/root-pin retirement。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/unix_rights_fixture/mod.rs"]
mod socket;
#[path = "../src/sync.rs"]
mod sync;
mod id {
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    pub(crate) fn next_runtime_object_id() -> u64 {
        NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1
    }
}
#[path = "../src/unix_rights_fixture/file.rs"]
mod file;
use alloc::{sync::Arc, vec};
use socket::{SocketError, UnixPassedFile, UnixRights, UnixSocket};
fn endpoint() -> (Arc<UnixSocket>, Arc<file::OpenFileDescription>) {
    let socket = UnixSocket::new();
    let file = Arc::new(file::OpenFileDescription {
        kind: file::OpenFileKind::Socket(Arc::new(socket::Socket {
            endpoint: socket.clone(),
        })),
    });
    socket.bind_file(&file);
    (socket, file)
}
fn rights(files: &[Arc<file::OpenFileDescription>], uid: u32) -> UnixRights {
    let files = files
        .iter()
        .cloned()
        .map(|file| -> Arc<dyn UnixPassedFile> { file })
        .collect();
    UnixRights::new(files, uid, Some(socket::ProducedLimit::new(usize::MAX))).unwrap()
}
#[test]
fn prepared_duplicate_edges_publish_without_allocations_and_detach_one_actual_batch() {
    let (target, file) = endpoint();
    let mut rights = rights(&[file.clone(), file.clone(), file.clone()], 8101);
    UnixSocket::prepare(&mut rights, &target).unwrap();
    UnixSocket::attach_without_allocation(&mut rights, &target);
    let restored = rights.into_files();
    assert_eq!(restored.len(), 3);
    assert!(restored.iter().all(|passed| passed.unix_node().is_some()));
    assert_eq!(UnixSocket::collect(8101, 0), Ok(()));
}
#[test]
fn preparation_oom_preserves_rights_and_does_not_consume_uid_quota() {
    let (target, file) = endpoint();
    for fault in 0..3 {
        let mut pending = rights(core::slice::from_ref(&file), 8102);
        allocation_failure::fail_after(fault);
        let result = UnixSocket::prepare(&mut pending, &target);
        let consumed = allocation_failure::was_consumed();
        assert_eq!(result, Err(SocketError::NoMemory));
        assert!(consumed);
        assert_eq!(pending.len(), 1);
        let files = pending.into_files();
        assert_eq!(files.len(), 1);
    }
    let mut pending = rights(core::slice::from_ref(&file), 8102);
    UnixSocket::prepare(&mut pending, &target).unwrap();
    UnixSocket::attach_without_allocation(&mut pending, &target);
    assert_eq!(pending.into_files().len(), 1);
}
#[test]
fn self_cycle_collects_without_nested_node_lock_or_ofd_drop_under_graph_guard() {
    let (target, file) = endpoint();
    let weak = Arc::downgrade(&file);
    target.enqueue(rights(core::slice::from_ref(&file), 8103));
    drop(file);
    UnixSocket::collect(8103, 0).unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(UnixSocket::collect(8103, 0), Ok(()));
}
#[test]
fn live_receiver_root_propagates_to_cycle_then_both_files_retire_after_last_root_closes() {
    let (a, file_a) = endpoint();
    let (b, file_b) = endpoint();
    let weak_a = Arc::downgrade(&file_a);
    let weak_b = Arc::downgrade(&file_b);
    a.enqueue(rights(core::slice::from_ref(&file_b), 8104));
    b.enqueue(rights(core::slice::from_ref(&file_a), 8104));
    drop(file_a);
    assert_eq!(
        UnixSocket::collect(8104, 0),
        Err(SocketError::TooManyReferences)
    );
    assert!(weak_a.upgrade().is_some());
    drop(file_b);
    UnixSocket::collect(8104, 0).unwrap();
    assert!(weak_a.upgrade().is_none());
    assert!(weak_b.upgrade().is_none());
}

#[test]
fn normal_worker_reclaims_below_limit_cycles_without_quota_recovery_or_new_allocations() {
    let (dead, file_dead) = endpoint();
    let (live, file_live) = endpoint();
    let weak = Arc::downgrade(&file_dead);
    dead.enqueue(rights(core::slice::from_ref(&file_dead), 8105));
    live.enqueue(rights(core::slice::from_ref(&file_live), 8105));
    drop(file_dead);
    let gate = sync::TaskMutexWaitPreparation::prepare().unwrap();
    let wait_target = interruptible_target::Target::new();
    let _binding = wait_target.bind();
    allocation_failure::fail_next();
    let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        UnixSocket::run_worker(
            || 0,
            |deadline| {
                let consumed = allocation_failure::was_consumed();
                assert!(!consumed, "actual collector pass allocated under pressure");
                assert_eq!(deadline, 100_000_000);
                panic!("host sleeper terminates infinite worker after one real pass");
            },
            gate,
        )
    }));
    assert!(stopped.is_err());
    assert!(weak.upgrade().is_none());
    live.release_queue();
}
#[test]
fn inflight_limit_checks_existing_usage_and_privileged_none_bypasses_the_limit() {
    let (target, file) = endpoint();
    let mut first = UnixRights::new(
        vec![file.clone(), file.clone(), file.clone()],
        8106,
        Some(socket::ProducedLimit::new(0)),
    )
    .unwrap();
    UnixSocket::prepare(&mut first, &target).unwrap();
    UnixSocket::publish_prepared(&mut first, &target);
    let mut second = UnixRights::new(
        vec![file.clone()],
        8106,
        Some(socket::ProducedLimit::new(0)),
    )
    .unwrap();
    UnixSocket::prepare(&mut second, &target).unwrap();
    assert!(UnixSocket::quota_rejected(&mut second, &target));
    let mut privileged = UnixRights::new(vec![file.clone()], 8106, None).unwrap();
    UnixSocket::prepare(&mut privileged, &target).unwrap();
    UnixSocket::publish_prepared(&mut privileged, &target);
    assert_eq!(first.into_files().len(), 3);
    assert_eq!(privileged.into_files().len(), 1);
}

#[path = "../src/interruptible_event_target.rs"]
mod interruptible_target;

#[test]
fn stream_reads_complete_carrying_write_and_delivers_each_barrier_once() {
    let (target, file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let mut first = Some(rights(core::slice::from_ref(&file), 8107));
    let mut second = Some(rights(core::slice::from_ref(&file), 8107));
    assert!(matches!(
        channel.write(b"first", &mut first).unwrap(),
        ipc::PipeWrite::Bytes(5)
    ));
    assert!(first.is_none());
    assert!(matches!(
        channel.write(b"second", &mut second).unwrap(),
        ipc::PipeWrite::Bytes(6)
    ));
    let mut bytes = [0u8; 32];
    let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
    let (read, control) = channel.read(&mut output, true);
    assert!(matches!(read, ipc::PipeRead::Bytes(5)));
    assert_eq!(control.unwrap().into_files().len(), 1);
    assert_eq!(&bytes[..5], b"first");
    let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
    let (read, control) = channel.read(&mut output, true);
    assert!(matches!(read, ipc::PipeRead::Bytes(6)));
    assert_eq!(control.unwrap().into_files().len(), 1);
    assert_eq!(&bytes[..6], b"second");
}
#[test]
fn short_stream_read_consumes_rights_once_and_ordinary_read_retires_them_outside_direction_guard() {
    let (target, file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let mut message = Some(rights(core::slice::from_ref(&file), 8108));
    channel.write(b"hello", &mut message).unwrap();
    let mut bytes = [0u8; 1];
    let (read, control) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut bytes), true);
    assert!(matches!(read, ipc::PipeRead::Bytes(1)));
    assert_eq!(control.unwrap().into_files().len(), 1);
    let mut rest = [0u8; 32];
    let (read, control) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut rest), true);
    assert!(matches!(read, ipc::PipeRead::Bytes(4)));
    assert!(control.is_none());
    assert_eq!(&rest[..4], b"ello");
    let mut message = Some(rights(core::slice::from_ref(&file), 8108));
    channel.write(b"data", &mut message).unwrap();
    let (read, control) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut rest), false);
    assert!(matches!(read, ipc::PipeRead::Bytes(4)));
    assert!(control.is_none());
    assert_eq!(UnixSocket::collect(8108, 0), Ok(()));
}

#[test]
fn stream_peek_copies_actual_rights_without_consuming_bytes_quota_or_edges() {
    let (target, file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let limit = socket::ProducedLimit::new(0);
    let mut first = Some(UnixRights::new(vec![file.clone()], 8116, Some(limit.clone())).unwrap());
    channel.write(b"hello", &mut first).unwrap();
    let mut pending = UnixRights::new(vec![file.clone()], 8116, Some(limit)).unwrap();
    UnixSocket::prepare(&mut pending, &target).unwrap();
    assert!(UnixSocket::quota_rejected(&mut pending, &target));
    let generation = channel.generations();
    let mut bytes = [0u8; 3];
    let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
    allocation_failure::fail_next();
    assert!(matches!(
        channel.peek(&mut output, true),
        Err(SocketError::NoMemory)
    ));
    assert!(allocation_failure::was_consumed());
    assert_eq!(output.len(), 0);
    assert_eq!(channel.generations(), generation);
    assert!(UnixSocket::quota_rejected(&mut pending, &target));
    for _ in 0..2 {
        let (read, rights) = channel
            .peek(&mut ipc::ReceiveBuffer::from_slice(&mut bytes), true)
            .unwrap();
        assert!(matches!(read, ipc::PipeRead::Bytes(3)));
        assert_eq!(&bytes, b"hel");
        assert_eq!(rights.unwrap().into_files().len(), 1);
        assert_eq!(channel.generations(), generation);
        assert!(UnixSocket::quota_rejected(&mut pending, &target));
    }
    allocation_failure::fail_next();
    let (_, rights) = channel
        .peek(&mut ipc::ReceiveBuffer::from_slice(&mut bytes), false)
        .unwrap();
    assert!(
        !allocation_failure::was_consumed(),
        "ordinary peek does not allocate ancillary storage"
    );
    assert!(rights.is_none());
    let mut all = [0u8; 16];
    let (read, rights) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut all), true);
    assert!(matches!(read, ipc::PipeRead::Bytes(5)));
    assert_eq!(&all[..5], b"hello");
    assert_eq!(rights.unwrap().into_files().len(), 1);
    assert!(!UnixSocket::quota_rejected(&mut pending, &target));
}

fn file_for(socket: &Arc<UnixSocket>) -> Arc<file::OpenFileDescription> {
    let file = Arc::new(file::OpenFileDescription {
        kind: file::OpenFileKind::Socket(Arc::new(socket::Socket {
            endpoint: socket.clone(),
        })),
    });
    socket.bind_file(&file);
    file
}

#[test]
fn pending_connection_listener_cycle_has_no_conservative_creator_root() {
    let listener = UnixSocket::listener();
    let listener_file = file_for(&listener);
    let weak = Arc::downgrade(&listener_file);
    let server = UnixSocket::new();
    UnixSocket::add_pending(&listener, &server);
    server.enqueue(rights(core::slice::from_ref(&listener_file), 8110));
    drop(listener_file);
    UnixSocket::collect(8110, 0).unwrap();
    assert!(weak.upgrade().is_none());
    assert!(listener.closed());
    assert!(
        server.closed(),
        "pending removal must close child despite observer Arc"
    );
}

#[test]
fn actual_listener_pin_prevents_collection_during_connect_preparation() {
    let listener = UnixSocket::listener();
    let listener_file = file_for(&listener);
    let weak = Arc::downgrade(&listener_file);
    let pin = listener.pin_file().unwrap();
    let server = UnixSocket::new();
    UnixSocket::add_pending(&listener, &server);
    server.enqueue(rights(core::slice::from_ref(&listener_file), 8111));
    drop(listener_file);
    assert_eq!(
        UnixSocket::collect(8111, 0),
        Err(SocketError::TooManyReferences)
    );
    assert!(!server.closed());
    drop(pin);
    UnixSocket::collect(8111, 0).unwrap();
    assert!(weak.upgrade().is_none());
    assert!(server.closed());
    assert!(
        listener.pin_file().is_none(),
        "retired Weak must never resurrect listener"
    );
}

#[test]
fn accepted_connection_root_replaces_backlog_edge_and_survives_listener_close() {
    let listener = UnixSocket::listener();
    let listener_file = file_for(&listener);
    let server = UnixSocket::new();
    UnixSocket::add_pending(&listener, &server);
    let accepted = UnixSocket::accept_pending(&listener);
    let accepted_file = file_for(&accepted);
    let weak_listener = Arc::downgrade(&listener_file);
    accepted.enqueue(rights(core::slice::from_ref(&listener_file), 8112));
    drop(listener_file);
    assert_eq!(
        UnixSocket::collect(8112, 0),
        Err(SocketError::TooManyReferences)
    );
    assert!(!accepted.closed());
    allocation_failure::fail_next();
    drop(accepted_file);
    assert!(
        !allocation_failure::was_consumed(),
        "close must not allocate"
    );
    assert!(accepted.closed());
    assert!(weak_listener.upgrade().is_none());
    assert_eq!(UnixSocket::collect(8112, 0), Ok(()));
}

#[test]
fn last_file_close_retires_stream_controls_while_transport_observers_remain() {
    let (target, target_file) = endpoint();
    let stream = UnixSocket::stream_channel(&target);
    let (_passed, passed_file) = endpoint();
    let weak_passed = Arc::downgrade(&passed_file);
    let mut controls = Some(rights(core::slice::from_ref(&passed_file), 8113));
    assert!(matches!(
        stream.write(b"pending", &mut controls).unwrap(),
        ipc::PipeWrite::Bytes(7)
    ));
    drop(passed_file);
    drop(target_file);
    assert!(target.closed());
    assert!(
        weak_passed.upgrade().is_none(),
        "queued rights cannot survive cached transmit Direction"
    );
    let mut none = None;
    assert!(matches!(
        stream.write(b"later", &mut none).unwrap(),
        ipc::PipeWrite::Broken
    ));
    assert_eq!(UnixSocket::collect(8113, 0), Ok(()));
}

#[test]
fn read_shutdown_retires_controls_and_closes_reader_with_cached_receive_arc() {
    let (target, _target_file) = endpoint();
    let stream = UnixSocket::stream_channel(&target);
    let (_passed, passed_file) = endpoint();
    let weak_passed = Arc::downgrade(&passed_file);
    let mut controls = Some(rights(core::slice::from_ref(&passed_file), 8114));
    stream.write(b"queued", &mut controls).unwrap();
    drop(passed_file);
    stream.close_receive();
    assert!(weak_passed.upgrade().is_none());
    let mut bytes = [0; 16];
    let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
    assert!(matches!(
        stream.read(&mut output, true).0,
        ipc::PipeRead::Eof
    ));
    assert!(matches!(
        stream.write(b"x", &mut None).unwrap(),
        ipc::PipeWrite::Broken
    ));
}

#[test]
fn prepared_scm_commit_reads_current_limit_instead_of_preparation_snapshot() {
    let (target, file) = endpoint();
    let quota = socket::ProducedLimit::new(0);
    let mut first = UnixRights::new(vec![file.clone()], 8115, Some(quota.clone())).unwrap();
    UnixSocket::prepare(&mut first, &target).unwrap();
    UnixSocket::publish_prepared(&mut first, &target);
    let mut pending = UnixRights::new(vec![file.clone()], 8115, Some(quota.clone())).unwrap();
    UnixSocket::prepare(&mut pending, &target).unwrap();
    assert!(UnixSocket::quota_rejected(&mut pending, &target));
    quota.set(1);
    UnixSocket::attach_without_allocation(&mut pending, &target);
    let mut third = UnixRights::new(vec![file], 8115, Some(quota.clone())).unwrap();
    quota.set(10);
    UnixSocket::prepare(&mut third, &target).unwrap();
    quota.set(0);
    assert!(UnixSocket::quota_rejected(&mut third, &target));
    drop(pending);
    drop(first);
    assert!(!UnixSocket::quota_rejected(&mut third, &target));
}

fn credential(key: usize, uid: u32) -> (socket::UnixCredentials, namespace::PidRegistration) {
    namespace::boot();
    let mut pid = namespace::PreparedPid::try_new(
        key,
        namespace::initial_pid(),
        &[],
        &crate::namespace::root_authority(),
    )
    .unwrap();
    pid.publish().unwrap();
    let registration = pid.take_registration();
    (
        socket::UnixCredentials {
            pid: pid.identity(),
            uid,
            gid: uid + 1,
        },
        registration,
    )
}

#[test]
fn stream_actual_chunks_coalesce_reads_without_allocation_and_preserve_native_boundaries() {
    let (target, _file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let (first, _first_registration) = credential(72300, 1000);
    let (second, _second_registration) = credential(72301, 2000);
    channel
        .write_credentials(b"a", &mut None, Some(&first))
        .unwrap();
    allocation_failure::fail_next();
    let failed = channel.write_credentials(b"b", &mut None, Some(&first));
    assert!(allocation_failure::was_consumed());
    assert!(matches!(failed, Err(SocketError::NoMemory)));
    channel
        .write_credentials(b"b", &mut None, Some(&first))
        .unwrap();
    channel
        .write_credentials(b"c", &mut None, Some(&second))
        .unwrap();
    channel.write(b"d", &mut None).unwrap();
    channel
        .write_credentials(b"e", &mut None, Some(&first))
        .unwrap();
    let mut bytes = [0u8; 16];
    for (expected, identity) in [
        (b"ab".as_slice(), Some(&first)),
        (b"c", Some(&second)),
        (b"d", None),
        (b"e", Some(&first)),
    ] {
        allocation_failure::fail_next();
        let (read, rights, observed) = channel
            .read_credentials(
                &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                true,
                false,
                true,
            )
            .unwrap();
        assert!(
            !allocation_failure::was_consumed(),
            "adjacent chunk read and completed node retirement allocate nothing"
        );
        assert!(matches!(read, ipc::PipeRead::Bytes(count) if count == expected.len()));
        assert_eq!(&bytes[..expected.len()], expected);
        assert!(rights.is_none());
        match (observed, identity) {
            (Some(observed), Some(expected)) => {
                assert!(Arc::ptr_eq(&observed.pid, &expected.pid));
                assert_eq!((observed.uid, observed.gid), (expected.uid, expected.gid));
            }
            (None, None) => {}
            _ => panic!("credentials borrowed from a later byte span"),
        }
    }
}

#[test]
fn stream_plain_prefix_peek_does_not_allocate_or_borrow_later_controls() {
    let (target, _target_file) = endpoint();
    let (_, passed) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    channel.write(b"plain", &mut None).unwrap();
    let (sender, _registration) = credential(72302, 1000);
    let mut rights = Some(rights(&[passed], 82302));
    channel
        .write_credentials(b"control", &mut rights, Some(&sender))
        .unwrap();
    let mut bytes = [0u8; 3];
    allocation_failure::fail_next();
    let (read, controls, credentials) = channel
        .read_credentials(
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            true,
            true,
            true,
        )
        .unwrap();
    assert!(
        !allocation_failure::was_consumed(),
        "prefix does not reach any rights"
    );
    assert!(matches!(read, ipc::PipeRead::Bytes(3)));
    assert_eq!(&bytes, b"pla");
    assert!(controls.is_none() && credentials.is_none());
    let mut all = [0u8; 32];
    let (read, controls, credentials) = channel
        .read_credentials(
            &mut ipc::ReceiveBuffer::from_slice(&mut all),
            true,
            false,
            true,
        )
        .unwrap();
    assert!(matches!(read, ipc::PipeRead::Bytes(5)));
    assert_eq!(&all[..5], b"plain");
    assert!(controls.is_none() && credentials.is_none());
    let (read, controls, credentials) = channel
        .read_credentials(
            &mut ipc::ReceiveBuffer::from_slice(&mut all),
            true,
            false,
            true,
        )
        .unwrap();
    assert!(matches!(read, ipc::PipeRead::Bytes(7)));
    assert_eq!(&all[..7], b"control");
    assert_eq!(controls.unwrap().into_files().len(), 1);
    assert!(Arc::ptr_eq(&credentials.unwrap().pid, &sender.pid));
}

#[test]
fn stream_zero_length_recvmsg_peeks_rights_then_detaches_once_without_consuming_bytes() {
    let (target, _target_file) = endpoint();
    let (_, passed) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let (sender, _registration) = credential(72303, 1000);
    let mut controls = Some(rights(&[passed], 82303));
    channel
        .write_credentials(b"payload", &mut controls, Some(&sender))
        .unwrap();
    let generation = channel.generations();
    for peek in [true, true, false] {
        let (read, rights, credentials) = channel
            .read_credentials(
                &mut ipc::ReceiveBuffer::from_slice(&mut []),
                true,
                peek,
                true,
            )
            .unwrap();
        assert!(matches!(read, ipc::PipeRead::Bytes(0)));
        assert_eq!(rights.unwrap().into_files().len(), 1);
        assert!(Arc::ptr_eq(&credentials.unwrap().pid, &sender.pid));
        assert_eq!(generation, channel.generations());
    }
    let (read, rights, credentials) = channel
        .read_credentials(
            &mut ipc::ReceiveBuffer::from_slice(&mut []),
            true,
            false,
            true,
        )
        .unwrap();
    assert!(matches!(read, ipc::PipeRead::Bytes(0)));
    assert!(rights.is_none());
    assert!(credentials.is_some());
    let mut bytes = [0u8; 32];
    let (read, rights, credentials) = channel
        .read_credentials(
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            true,
            false,
            true,
        )
        .unwrap();
    assert!(matches!(read, ipc::PipeRead::Bytes(7)));
    assert_eq!(&bytes[..7], b"payload");
    assert!(rights.is_none() && credentials.is_some());
}

#[test]
fn stream_ordinary_read_crosses_credentials_and_retains_partial_span_identity() {
    let (target, _target_file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let (first, _first_registration) = credential(72304, 1000);
    let (second, _second_registration) = credential(72305, 2000);
    channel
        .write_credentials(b"aaa", &mut None, Some(&first))
        .unwrap();
    channel
        .write_credentials(b"bbb", &mut None, Some(&second))
        .unwrap();
    let mut bytes = [0u8; 4];
    let (read, rights) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut bytes), false);
    assert!(matches!(read, ipc::PipeRead::Bytes(4)));
    assert_eq!(&bytes, b"aaab");
    assert!(rights.is_none());
    let (read, _, credentials) = channel
        .read_credentials(
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            true,
            false,
            true,
        )
        .unwrap();
    assert!(matches!(read, ipc::PipeRead::Bytes(2)));
    assert_eq!(&bytes[..2], b"bb");
    assert!(Arc::ptr_eq(&credentials.unwrap().pid, &second.pid));
}

#[test]
fn stream_fifo_preserves_many_chunks_and_reuses_empty_tail_after_oom() {
    let (target, _file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    for round in 0..3u8 {
        for index in 0..64u8 {
            assert!(matches!(
                channel.write(&[round, index], &mut None).unwrap(),
                ipc::PipeWrite::Bytes(2)
            ));
        }
        let mut bytes = [0; 128];
        let mut output = ipc::ReceiveBuffer::from_slice(&mut bytes);
        let (read, controls) = channel.read(&mut output, false);
        assert!(matches!(read, ipc::PipeRead::Bytes(128)));
        assert!(controls.is_none());
        for (index, chunk) in bytes.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(*chunk, [round, index as u8]);
        }
        allocation_failure::fail_after(1);
        assert!(matches!(
            channel.write(b"x", &mut None),
            Err(SocketError::NoMemory)
        ));
        assert!(allocation_failure::was_consumed());
        let mut empty = [0; 1];
        let mut output = ipc::ReceiveBuffer::from_slice(&mut empty);
        assert!(matches!(
            channel.read(&mut output, false).0,
            ipc::PipeRead::Empty
        ));
    }
    channel.write(b"last", &mut None).unwrap();
    channel.close_receive();
    assert!(matches!(
        channel.write(b"closed", &mut None).unwrap(),
        ipc::PipeWrite::Broken
    ));
}

#[test]
fn stream_control_and_frame_oom_preserve_caller_rights_before_publication() {
    let (target, file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let mut control = rights(core::slice::from_ref(&file), 8191);
    UnixSocket::prepare(&mut control, &target).unwrap();
    let mut control = Some(control);
    for fault in [1, 2] {
        allocation_failure::fail_after(fault);
        assert!(matches!(
            channel.write(b"fd", &mut control),
            Err(SocketError::NoMemory)
        ));
        assert!(allocation_failure::was_consumed());
        assert!(control.is_some());
        let mut bytes = [0; 2];
        assert!(matches!(
            channel
                .read(&mut ipc::ReceiveBuffer::from_slice(&mut bytes), true)
                .0,
            ipc::PipeRead::Empty
        ));
    }
    channel.write(b"fd", &mut control).unwrap();
    assert!(control.is_none());
    let mut bytes = [0; 2];
    let (read, received) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut bytes), true);
    assert!(matches!(read, ipc::PipeRead::Bytes(2)));
    assert_eq!(&bytes, b"fd");
    assert_eq!(received.unwrap().into_files().len(), 1);
}

#[test]
fn partial_stream_control_detach_keeps_charged_storage_until_chunk_retirement() {
    let (target, file) = endpoint();
    let channel = UnixSocket::stream_channel(&target);
    let mut control = Some(rights(core::slice::from_ref(&file), 8192));
    channel.write(b"abc", &mut control).unwrap();
    assert_eq!(channel.control_storage_count(), 1);
    let mut first = [0; 1];
    let (read, received) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut first), true);
    assert!(matches!(read, ipc::PipeRead::Bytes(1)));
    assert_eq!(received.unwrap().into_files().len(), 1);
    assert_eq!(channel.control_storage_count(), 1);
    let mut rest = [0; 2];
    let (read, received) = channel.read(&mut ipc::ReceiveBuffer::from_slice(&mut rest), true);
    assert!(matches!(read, ipc::PipeRead::Bytes(2)));
    assert!(received.is_none());
    assert_eq!(&rest, b"bc");
    assert_eq!(channel.control_storage_count(), 0);
}
