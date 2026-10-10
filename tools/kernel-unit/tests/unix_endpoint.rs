#![feature(allocator_ext)]
#![allow(dead_code)]
//! 完整实际 endpoint/namespace/backlog/packet/frame/graph owners 的事务与生命周期回归。
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
mod socket;
#[path = "../src/sync.rs"]
mod sync;
use alloc::{sync::Arc, vec};
use socket::{SocketError, SocketType, UnixCredentials, UnixSendCredentials, UnixSocket};
fn endpoint(kind: SocketType) -> (Arc<UnixSocket>, Arc<file::OpenFileDescription>) {
    namespace::boot();
    let endpoint = UnixSocket::new(
        kind,
        ipc::Pipe::notification_pair().unwrap(),
        id::next_runtime_object_id(),
        socket::boot_net(),
    )
    .unwrap();
    let ofd = Arc::new(file::OpenFileDescription {
        kind: file::OpenFileKind::Socket(Arc::new(socket::Socket {
            endpoint: endpoint.clone(),
        })),
    });
    socket::bind_file(&endpoint, &ofd);
    (endpoint, ofd)
}
fn credential(key: usize) -> (UnixCredentials, namespace::PidRegistration) {
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
        UnixCredentials {
            pid: pid.identity(),
            uid: 1000,
            gid: 2000,
        },
        registration,
    )
}
fn listener(
    kind: SocketType,
    label: &[u8],
    credentials: &UnixCredentials,
) -> (Arc<UnixSocket>, Arc<file::OpenFileDescription>) {
    let (endpoint, ofd) = endpoint(kind);
    endpoint
        .bind(socket::UnixAddress::new(label).unwrap())
        .unwrap();
    endpoint.listen(0, credentials.clone()).unwrap();
    (endpoint, ofd)
}
fn resources(kind: SocketType) -> Result<socket::UnixConnectResources, SocketError> {
    Ok(socket::UnixConnectResources {
        server_notify: ipc::Pipe::notification_pair().unwrap(),
        data: if kind == SocketType::Stream {
            socket::UnixConnectionData::Stream {
                first_to_second: ipc::Pipe::notification_pair().unwrap(),
                second_to_first: ipc::Pipe::notification_pair().unwrap(),
            }
        } else {
            socket::UnixConnectionData::Packet
        },
    })
}
fn accepted(listener: &UnixSocket) -> (Arc<UnixSocket>, Arc<file::OpenFileDescription>) {
    let endpoint = listener.accept().unwrap();
    let ofd = Arc::new(file::OpenFileDescription {
        kind: file::OpenFileKind::Socket(Arc::new(socket::Socket {
            endpoint: endpoint.clone(),
        })),
    });
    socket::bind_file(&endpoint, &ofd);
    (endpoint, ofd)
}
#[test]
fn oriented_connect_preserves_rollback_and_backlog_admission_before_resources() {
    let (credentials, _registration) = credential(72400);
    for (kind, label) in [
        (SocketType::Stream, b"\0actual-stream-lifecycle".as_slice()),
        (SocketType::SeqPacket, b"\0actual-seq-lifecycle".as_slice()),
    ] {
        let (listener, _listener_file) = listener(kind, label, &credentials);
        let (client, _client_file) = endpoint(kind);
        assert!(matches!(
            UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || Err(
                SocketError::NoMemory
            )),
            Err(SocketError::NoMemory)
        ));
        assert!(!listener.poll_state().readable);
        UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || resources(kind))
            .unwrap();
        assert!(listener.poll_state().readable);
        let (next, _next_file) = endpoint(kind);
        assert!(matches!(
            UnixSocket::connect_oriented(&next, &listener, credentials.clone(), || panic!(
                "full backlog cannot invoke factory"
            )),
            Err(SocketError::Again)
        ));
        let (_server, _server_file) = accepted(&listener);
        UnixSocket::connect_oriented(&next, &listener, credentials.clone(), || resources(kind))
            .unwrap();
        let (_second_server, _second_server_file) = accepted(&listener);
    }
}
#[test]
fn seqpacket_pending_messages_credentials_and_half_close_drain_one_actual_queue() {
    let (credentials, _registration) = credential(72401);
    let (listener, listener_file) = listener(
        SocketType::SeqPacket,
        b"\0seq-queued-before-accept",
        &credentials,
    );
    listener.set_pass_credentials(true);
    let (client, client_file) = endpoint(SocketType::SeqPacket);
    UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || {
        resources(SocketType::SeqPacket)
    })
    .unwrap();
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    assert!(matches!(
        client.send_to_with_ancillary(b"first", None, &mut None, &sender),
        Ok(5)
    ));
    assert!(matches!(
        client.send_to_with_ancillary(b"second", None, &mut None, &sender),
        Ok(6)
    ));
    client.shutdown(1).unwrap();
    let (server, server_file) = accepted(&listener);
    assert!(server.pass_credentials());
    let mut bytes = [0u8; 3];
    for peek in [true, false] {
        let (count, full, _, rights, actual) = socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            peek,
            true,
            true,
        )
        .unwrap();
        assert_eq!((count, full), (3, 5));
        assert_eq!(&bytes, b"fir");
        assert!(rights.is_none());
        assert!(Arc::ptr_eq(&actual.unwrap().pid, &credentials.pid));
    }
    let (count, full, _, _, _) = socket::receive(
        &server,
        &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
        false,
        true,
        true,
    )
    .unwrap();
    assert_eq!((count, full), (3, 6));
    assert_eq!(&bytes, b"sec");
    assert_eq!(
        socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            true,
            true
        )
        .unwrap()
        .0,
        0
    );
    assert!(matches!(
        client.send_to_with_ancillary(b"closed", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
    assert!(matches!(
        server.send_to_with_ancillary(b"reply", None, &mut None, &sender),
        Ok(5)
    ));
    server.shutdown(2).unwrap();
    assert!(server.poll_state().hangup);
    drop(server_file);
    assert!(client.poll_state().hangup);
    drop(client_file);
    drop(listener_file);
}
#[test]
fn queued_datagram_keeps_actual_sender_address_after_late_bind_and_fput() {
    let (credentials, _registration) = credential(72402);
    let sender = UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    let (source, source_file) = endpoint(SocketType::Datagram);
    let (target, _target_file) = endpoint(SocketType::Datagram);
    assert!(matches!(
        source.send_to_with_ancillary(b"queued", Some(&target), &mut None, &sender),
        Ok(6)
    ));
    let address = socket::UnixAddress::new(b"\0bound-after-send").unwrap();
    source.bind(address).unwrap();
    drop(source_file);
    let (replacement, _replacement_file) = endpoint(SocketType::Datagram);
    replacement.bind(address).unwrap();
    let mut bytes = [0u8; 16];
    let (count, full, actual, _, _) = socket::receive(
        &target,
        &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
        true,
        false,
        true,
    )
    .unwrap();
    assert_eq!((count, full, actual), (6, 6, Some(address)));
    assert_eq!(&bytes[..count], b"queued");
    let (_, _, actual, _, _) = socket::receive(
        &target,
        &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
        false,
        false,
        true,
    )
    .unwrap();
    assert_eq!(actual, Some(address));
}
#[test]
fn datagram_connected_peer_filter_and_native_read_shutdown_policy() {
    let (credentials, _registration) = credential(72403);
    let sender = UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    let (first, _first_file) = endpoint(SocketType::Datagram);
    let (second, _second_file) = endpoint(SocketType::Datagram);
    let (stranger, _stranger_file) = endpoint(SocketType::Datagram);
    second.connect_datagram(&first).unwrap();
    assert!(matches!(
        stranger.send_to_with_ancillary(b"rejected", Some(&second), &mut None, &sender),
        Err(socket::SocketSendError::Error(
            SocketError::OperationNotPermitted
        ))
    ));
    assert!(matches!(
        stranger.connect_datagram(&second),
        Err(SocketError::OperationNotPermitted)
    ));
    assert!(matches!(
        first.send_to_with_ancillary(b"accepted", Some(&second), &mut None, &sender),
        Ok(8)
    ));
    second.shutdown(0).unwrap();
    assert!(matches!(
        first.send_to_with_ancillary(b"closed", Some(&second), &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
    let mut bytes = [0u8; 16];
    assert_eq!(
        socket::receive(
            &second,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            false,
            true
        )
        .unwrap()
        .0,
        8
    );
    assert!(matches!(
        socket::receive(
            &second,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            false,
            true
        ),
        Err(SocketError::Again)
    ));
    assert_eq!(
        socket::receive(
            &second,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            false,
            false
        )
        .unwrap()
        .0,
        0
    );
}

#[test]
fn oriented_close_reports_reset_once_and_stream_drains_queued_prefix_first() {
    let (credentials, _registration) = credential(72404);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    for (kind, label) in [
        (SocketType::Stream, b"\0close-reset-stream".as_slice()),
        (SocketType::SeqPacket, b"\0close-reset-seq".as_slice()),
    ] {
        let (listener, _listener_file) = listener(kind, label, &credentials);
        let (client, _client_file) = endpoint(kind);
        UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || resources(kind))
            .unwrap();
        let (server, server_file) = accepted(&listener);
        assert!(matches!(
            client.send_to_with_ancillary(b"unread", None, &mut None, &sender),
            Ok(6)
        ));
        assert!(matches!(
            server.send_to_with_ancillary(b"queued", None, &mut None, &sender),
            Ok(6)
        ));
        drop(server_file);
        assert!(client.poll_state().error);
        let mut bytes = [0; 16];
        if kind == SocketType::Stream {
            assert_eq!(
                socket::receive(
                    &client,
                    &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                    false,
                    true,
                    true
                )
                .unwrap()
                .0,
                6
            );
            assert_eq!(&bytes[..6], b"queued");
            assert!(client.poll_state().error);
        }
        assert!(matches!(
            socket::receive(
                &client,
                &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                false,
                true,
                true
            ),
            Err(SocketError::ConnectionReset)
        ));
        assert!(!client.poll_state().error);
        assert_eq!(client.take_error(), None);
        if kind == SocketType::SeqPacket {
            assert_eq!(
                socket::receive(
                    &client,
                    &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                    false,
                    true,
                    true
                )
                .unwrap()
                .0,
                6
            );
        }
        assert_eq!(
            socket::receive(
                &client,
                &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                false,
                true,
                true
            )
            .unwrap()
            .0,
            0
        );
    }
}

#[test]
fn unaccepted_empty_connection_close_reports_reset_and_so_error_consumes_it() {
    let (credentials, _registration) = credential(72405);
    for (kind, label) in [
        (SocketType::Stream, b"\0embryo-stream".as_slice()),
        (SocketType::SeqPacket, b"\0embryo-seq".as_slice()),
    ] {
        let (listener, listener_file) = listener(kind, label, &credentials);
        let (client, _client_file) = endpoint(kind);
        UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || resources(kind))
            .unwrap();
        drop(listener_file);
        assert!(client.poll_state().error);
        assert_eq!(client.take_error(), Some(SocketError::ConnectionReset));
        assert_eq!(client.take_error(), None);
        assert!(!client.poll_state().error);
        let mut bytes = [0; 1];
        assert_eq!(
            socket::receive(
                &client,
                &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                false,
                true,
                true
            )
            .unwrap()
            .0,
            0
        );
    }
}

#[test]
fn stream_shutdown_read_preserves_queued_bytes_and_controls_until_drain() {
    let (credentials, _registration) = credential(72406);
    let (listener, _listener_file) =
        listener(SocketType::Stream, b"\0stream-shutdown-drain", &credentials);
    let (client, _client_file) = endpoint(SocketType::Stream);
    UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || {
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    server.set_pass_credentials(true);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    assert!(matches!(
        client.send_to_with_ancillary(b"history", None, &mut None, &sender),
        Ok(7)
    ));
    server.shutdown(0).unwrap();
    assert!(server.poll_state().readable);
    assert!(!server.poll_state().hangup);
    assert!(server.poll_state().read_closed);
    assert!(!client.poll_state().read_closed);
    assert!(matches!(
        client.send_to_with_ancillary(b"new", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
    let mut bytes = [0; 16];
    let result = socket::receive(
        &server,
        &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
        false,
        true,
        true,
    )
    .unwrap();
    assert_eq!(result.0, 7);
    assert_eq!(&bytes[..7], b"history");
    assert!(Arc::ptr_eq(&result.4.unwrap().pid, &credentials.pid));
    assert_eq!(
        socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            true,
            true
        )
        .unwrap()
        .0,
        0
    );
    server.shutdown(1).unwrap();
    assert!(server.poll_state().hangup);
}

#[test]
fn pathname_inode_is_pinned_by_live_endpoint_and_child_but_not_closed_observers() {
    let (credentials, _registration) = credential(72407);
    let (listener, listener_file) = endpoint(SocketType::SeqPacket);
    let inode = Arc::new(42u8);
    let witness = Arc::downgrade(&inode);
    let address = socket::UnixAddress::new(b"/tmp/actual-inode-owner").unwrap();
    let identity = socket::UnixPathIdentity {
        filesystem: 725,
        inode: 42,
    };
    listener
        .bind_path(address, identity, inode.clone())
        .unwrap();
    drop(inode);
    assert!(witness.upgrade().is_some());
    listener.listen(0, credentials.clone()).unwrap();
    let (client, _client_file) = endpoint(SocketType::SeqPacket);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::SeqPacket)
    })
    .unwrap();
    let (server, server_file) = accepted(&listener);
    drop(listener_file);
    assert!(
        witness.upgrade().is_some(),
        "accepted child owns its actual inode pin"
    );
    drop(server_file);
    assert!(
        witness.upgrade().is_none(),
        "backend/peer observers retain address only after fput"
    );
    assert_eq!(server.address(), Some(address));
    assert_eq!(client.peer_address().unwrap(), Some(address));
    assert!(matches!(
        UnixSocket::lookup_path(identity),
        Err(SocketError::ConnectionRefused)
    ));
}

#[test]
fn datagram_reconnect_purges_old_queue_and_reports_loss_to_mutual_peer() {
    let (credentials, _registration) = credential(72408);
    let sender = UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    let (source, _source_file) = endpoint(SocketType::Datagram);
    let (old, old_file) = endpoint(SocketType::Datagram);
    let (new, _new_file) = endpoint(SocketType::Datagram);
    source.connect_datagram(&old).unwrap();
    old.connect_datagram(&source).unwrap();
    assert!(matches!(
        old.send_to_with_ancillary(b"history", None, &mut None, &sender),
        Ok(7)
    ));
    source.connect_datagram(&old).unwrap();
    assert!(
        source.poll_state().readable,
        "reconnect to same actual peer preserves queue"
    );
    source.connect_datagram(&new).unwrap();
    assert!(!source.poll_state().readable);
    assert_eq!(old.take_error(), Some(SocketError::ConnectionReset));
    assert_eq!(old.take_error(), None);
    source.disconnect_datagram().unwrap();
    assert!(matches!(
        source.peer_address(),
        Err(SocketError::NotConnected)
    ));
    source.connect_datagram(&old).unwrap();
    assert!(matches!(
        old.send_to_with_ancillary(b"lost", None, &mut None, &sender),
        Ok(4)
    ));
    drop(old_file);
    assert!(matches!(
        source.send_to_with_ancillary(b"dead", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(
            SocketError::ConnectionRefused
        ))
    ));
    assert!(!source.poll_state().readable);
    assert!(matches!(
        source.send_to_with_ancillary(b"again", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::NotConnected))
    ));
}

#[test]
fn stream_preconnect_shutdown_write_is_not_replayed_as_peer_eof() {
    let (credentials, _registration) = credential(72409);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) =
        listener(SocketType::Stream, b"\0preconnect-shutdown", &credentials);
    let (client, client_file) = endpoint(SocketType::Stream);
    client.shutdown(1).unwrap();
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    assert!(matches!(
        client.send_to_with_ancillary(b"new", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
    let mut bytes = [0; 16];
    assert!(matches!(
        socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            true,
            true
        ),
        Err(SocketError::Again)
    ));
    assert!(matches!(
        server.send_to_with_ancillary(b"reply", None, &mut None, &sender),
        Ok(5)
    ));
    assert_eq!(
        socket::receive(
            &client,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            true,
            true
        )
        .unwrap()
        .0,
        5
    );
    drop(client_file);
    assert_eq!(
        socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            true,
            true
        )
        .unwrap()
        .0,
        0
    );
}

#[test]
fn listener_shutdown_accepts_existing_child_then_reports_invalid_on_empty_queue() {
    let (credentials, _registration) = credential(72410);
    let (listener, _listener_file) =
        listener(SocketType::SeqPacket, b"\0listener-shutdown", &credentials);
    let (client, _client_file) = endpoint(SocketType::SeqPacket);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::SeqPacket)
    })
    .unwrap();
    listener.shutdown(0).unwrap();
    let (_server, _server_file) = accepted(&listener);
    assert!(listener.poll_state().readable);
    assert!(matches!(listener.accept(), Err(SocketError::Invalid)));
}

#[test]
fn seqpacket_uses_sender_memory_instead_of_ten_message_limit_and_refunds_on_drain() {
    let (credentials, _registration) = credential(72411);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) = listener(
        SocketType::SeqPacket,
        b"\0packet-memory-ledger",
        &credentials,
    );
    let (client, _client_file) = endpoint(SocketType::SeqPacket);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::SeqPacket)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    let payload = vec![0x5a; 8 * 1024];
    let mut sent = 0;
    loop {
        match client.send_to_with_ancillary(&payload, None, &mut None, &sender) {
            Ok(count) => {
                assert_eq!(count, payload.len());
                sent += 1;
            }
            Err(socket::SocketSendError::Capacity(wait)) => {
                assert!(!wait.is_ready());
                assert!(
                    sent > 11,
                    "oriented packet admission is controlled by actual sender bytes"
                );
                assert!(!client.poll_state().writable);
                break;
            }
            _ => panic!("unexpected packet send result"),
        }
    }
    for _ in 0..sent {
        let mut output = ipc::ReceiveBuffer::deferred(usize::MAX);
        assert_eq!(
            socket::receive(&server, &mut output, false, false, true)
                .unwrap()
                .0,
            payload.len()
        );
        assert_eq!(output.initialized(), payload);
    }
    assert!(client.poll_state().writable);
    assert!(matches!(
        client.send_to_with_ancillary(&payload, None, &mut None, &sender),
        Ok(8192)
    ));
}

#[test]
fn packet_receive_prepares_actual_large_message_without_user_capacity_allocation_or_peek_refund() {
    let (credentials, _registration) = credential(72412);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) = listener(
        SocketType::SeqPacket,
        b"\0large-packet-staging",
        &credentials,
    );
    let (client, _client_file) = endpoint(SocketType::SeqPacket);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::SeqPacket)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    let payload = vec![0x79; 100_000];
    assert!(matches!(
        client.send_to_with_ancillary(&payload, None, &mut None, &sender),
        Ok(100_000)
    ));
    assert!(!client.poll_state().writable);
    let mut output = ipc::ReceiveBuffer::deferred(usize::MAX);
    allocation_failure::fail_next();
    let result = socket::receive(&server, &mut output, true, false, true);
    let consumed = allocation_failure::was_consumed();
    assert!(consumed);
    assert!(matches!(result, Err(SocketError::NoMemory)));
    assert_eq!(output.len(), 0);
    assert!(!client.poll_state().writable);
    assert_eq!(
        socket::receive(&server, &mut output, true, false, true)
            .unwrap()
            .0,
        payload.len()
    );
    assert_eq!(output.initialized(), payload);
    assert!(
        !client.poll_state().writable,
        "peek does not release the actual queued memory charge"
    );
    let mut output = ipc::ReceiveBuffer::deferred(payload.len());
    assert_eq!(
        socket::receive(&server, &mut output, false, false, true)
            .unwrap()
            .0,
        payload.len()
    );
    assert!(client.poll_state().writable);
}

#[test]
fn unconnected_packet_queue_admits_native_plus_one_and_self_send_uses_memory_budget() {
    let (credentials, _registration) = credential(72413);
    let sender = UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    let (source, _source_file) = endpoint(SocketType::Datagram);
    let (target, _target_file) = endpoint(SocketType::Datagram);
    for _ in 0..11 {
        assert!(matches!(
            source.send_to_with_ancillary(b"x", Some(&target), &mut None, &sender),
            Ok(1)
        ));
    }
    let wait = match source.send_to_with_ancillary(b"x", Some(&target), &mut None, &sender) {
        Err(socket::SocketSendError::Capacity(wait)) => wait,
        _ => panic!("twelfth unconnected packet must wait for target queue capacity"),
    };
    assert!(!wait.is_ready());
    let mut byte = [0];
    socket::receive(
        &target,
        &mut ipc::ReceiveBuffer::from_slice(&mut byte),
        false,
        false,
        true,
    )
    .unwrap();
    assert!(wait.is_ready());
    for _ in 0..12 {
        assert!(matches!(
            source.send_to_with_ancillary(b"self", Some(&source), &mut None, &sender),
            Ok(4)
        ));
    }
}

#[test]
fn buffer_policy_clamps_unsigned_inputs_and_new_children_keep_native_defaults() {
    use socket::SocketIoDirection::{Receive, Send};
    let (credentials, _registration) = credential(72414);
    let (listener, _listener_file) = listener(SocketType::Stream, b"\0buffer-policy", &credentials);
    assert_eq!(listener.buffer_limit(Send), 212_992);
    listener.set_buffer_limit(Send, 32_768, false);
    listener.set_buffer_limit(Receive, 16_384, false);
    assert_eq!(listener.buffer_limit(Send), 65_536);
    assert_eq!(listener.buffer_limit(Receive), 32_768);
    listener.set_buffer_limit(Send, -1, false);
    assert_eq!(listener.buffer_limit(Send), 8 << 20);
    listener.set_buffer_limit(Send, 0, false);
    listener.set_buffer_limit(Receive, 0, false);
    let minimum = listener.buffer_limit(Receive);
    assert!(minimum >= 2048);
    assert_eq!(listener.buffer_limit(Send), minimum * 2);
    let (client, _client_file) = endpoint(SocketType::Stream);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    assert_eq!(server.buffer_limit(Send), 212_992);
    assert_eq!(server.buffer_limit(Receive), 212_992);
}

#[test]
fn stream_sender_credit_tracks_partial_reads_peek_frames_oom_and_final_close() {
    use socket::SocketIoDirection::Send;
    let (credentials, _registration) = credential(72415);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) =
        listener(SocketType::Stream, b"\0stream-byte-budget", &credentials);
    let (client, _client_file) = endpoint(SocketType::Stream);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, server_file) = accepted(&listener);
    server.set_pass_credentials(true);
    client.set_buffer_limit(Send, 0, false);
    // 首个 frame 分配失败不能留下 provisional payload/node 费用。
    allocation_failure::fail_next();
    let result = client.send_to_with_ancillary(b"x", None, &mut None, &sender);
    assert!(allocation_failure::was_consumed());
    assert!(matches!(
        result,
        Err(socket::SocketSendError::Error(SocketError::NoMemory))
    ));
    assert!(client.poll_state().writable);
    let payload = vec![0x35; 8192];
    let mut queued = 0;
    let wait = loop {
        match client.send_to_with_ancillary(&payload, None, &mut None, &sender) {
            Ok(count) => {
                assert!(count > 0 && count < payload.len());
                queued += count;
            }
            Err(socket::SocketSendError::Capacity(wait)) => break wait,
            _ => panic!("unexpected stream credit result"),
        }
    };
    assert!(!wait.is_ready());
    assert!(!client.poll_state().writable);
    let mut bytes = [0; 97];
    for _ in 0..2 {
        let (_, _, _, _, credentials) = socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            true,
            false,
            true,
        )
        .unwrap();
        assert!(credentials.is_some());
        assert_eq!(bytes, [0x35; 97]);
        assert!(!wait.is_ready());
    }
    let consumed = socket::receive(
        &server,
        &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
        false,
        false,
        true,
    )
    .unwrap()
    .0;
    assert_eq!(consumed, bytes.len());
    queued -= consumed;
    assert!(
        !wait.is_ready(),
        "partial actual chunk retains full allocation credit"
    );
    assert!(!client.poll_state().writable);
    while queued != 0 {
        let count = socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            false,
            true,
        )
        .unwrap()
        .0;
        assert!(count > 0);
        queued -= count;
    }
    assert!(wait.is_ready());
    assert!(client.poll_state().writable);
    assert!(
        client
            .send_to_with_ancillary(&payload, None, &mut None, &sender)
            .is_ok()
    );
    assert!(!client.poll_state().writable);
    drop(server_file);
    assert!(
        client.poll_state().writable,
        "last receive OFD retires queued byte and frame fees"
    );
}

#[test]
fn packet_receive_buffer_hint_does_not_create_a_non_native_byte_limit() {
    let (credentials, _registration) = credential(72416);
    let sender = UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    let (source, _source_file) = endpoint(SocketType::Datagram);
    let (target, _target_file) = endpoint(SocketType::Datagram);
    target.set_buffer_limit(socket::SocketIoDirection::Receive, 0, false);
    let payload = vec![0x61; 100_000];
    assert!(matches!(
        source.send_to_with_ancillary(&payload, Some(&target), &mut None, &sender),
        Ok(100_000)
    ));
    let mut output = ipc::ReceiveBuffer::deferred(usize::MAX);
    assert_eq!(
        socket::receive(&target, &mut output, false, false, true)
            .unwrap()
            .0,
        payload.len()
    );
    assert_eq!(output.initialized(), payload);
}

#[test]
fn concurrent_connect_preparation_cannot_overwrite_or_reset_the_winning_connection() {
    let (credentials, _registration) = credential(72417);
    for (kind, first_name, second_name) in [
        (
            SocketType::Stream,
            b"\0connect-race-stream-a".as_slice(),
            b"\0connect-race-stream-b".as_slice(),
        ),
        (
            SocketType::SeqPacket,
            b"\0connect-race-seq-a".as_slice(),
            b"\0connect-race-seq-b".as_slice(),
        ),
    ] {
        let (first, _first_file) = listener(kind, first_name, &credentials);
        let (second, _second_file) = listener(kind, second_name, &credentials);
        let (client, _client_file) = endpoint(kind);
        let result = UnixSocket::connect_oriented(&client, &first, credentials.clone(), || {
            // 实际 owner 的资源准备窗口注入第二个 connect，而非复制 transaction 算法。
            UnixSocket::connect_oriented(&client, &second, credentials.clone(), || resources(kind))
                .unwrap();
            resources(kind)
        });
        assert!(matches!(result, Err(SocketError::AlreadyConnected)));
        assert!(!first.poll_state().readable);
        assert!(second.poll_state().readable);
        assert_eq!(
            client.take_error(),
            None,
            "losing unpublished child cannot reset winning peer"
        );
        assert!(!client.poll_state().hangup);
        let (server, _server_file) = accepted(&second);
        let sender = UnixSendCredentials {
            sender: credentials.clone(),
            claimed: None,
        };
        assert!(matches!(
            client.send_to_with_ancillary(b"winner", None, &mut None, &sender),
            Ok(6)
        ));
        let mut bytes = [0; 6];
        assert_eq!(
            socket::receive(
                &server,
                &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
                false,
                false,
                true
            )
            .unwrap()
            .0,
            6
        );
        assert_eq!(&bytes, b"winner");
        // 失败 reservation 必须释放真实 capacity，下一次 connect 可使用同一个 listener。
        let (next, _next_file) = endpoint(kind);
        UnixSocket::connect_oriented(&next, &first, credentials.clone(), || resources(kind))
            .unwrap();
        let (_accepted, _accepted_file) = accepted(&first);
    }
}

#[test]
fn connect_publication_observes_shutdown_during_preparation_and_rolls_back_listener_capacity() {
    let (credentials, _registration) = credential(72418);
    let (listener, _listener_file) =
        listener(SocketType::Stream, b"\0connect-late-shutdown", &credentials);
    let (client, _client_file) = endpoint(SocketType::Stream);
    UnixSocket::connect_oriented(&client, &listener, credentials.clone(), || {
        client.shutdown(1).unwrap();
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    assert!(matches!(
        client.send_to_with_ancillary(b"x", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
    let mut bytes = [0; 1];
    assert!(matches!(
        socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            false,
            true
        ),
        Err(SocketError::Again)
    ));
    let (next, _next_file) = endpoint(SocketType::Stream);
    let result = UnixSocket::connect_oriented(&next, &listener, credentials, || {
        listener.shutdown(0).unwrap();
        resources(SocketType::Stream)
    });
    assert!(matches!(result, Err(SocketError::ConnectionRefused)));
    assert!(!listener.poll_state().error);
    assert_eq!(next.take_error(), None);
    assert!(matches!(listener.accept(), Err(SocketError::Invalid)));
}

#[test]
fn explicit_closed_destination_requests_relookup_without_clearing_a_different_peer() {
    let (credentials, _registration) = credential(72419);
    let sender = UnixSendCredentials {
        sender: credentials,
        claimed: None,
    };
    let (source, _source_file) = endpoint(SocketType::Datagram);
    let (current, _current_file) = endpoint(SocketType::Datagram);
    let (closed, closed_file) = endpoint(SocketType::Datagram);
    source.connect_datagram(&current).unwrap();
    drop(closed_file);
    assert!(matches!(
        source.send_to_with_ancillary(b"retry", Some(&closed), &mut None, &sender),
        Err(socket::SocketSendError::DestinationChanged)
    ));
    assert!(matches!(
        source.send_to_with_ancillary(b"current", None, &mut None, &sender),
        Ok(7)
    ));
    let mut bytes = [0; 7];
    assert_eq!(
        socket::receive(
            &current,
            &mut ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
            false,
            true
        )
        .unwrap()
        .0,
        7
    );
    assert_eq!(&bytes, b"current");
}

#[test]
fn zero_stream_send_distinguishes_peer_read_shutdown_from_final_peer_close() {
    let (credentials, _registration) = credential(72420);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) =
        listener(SocketType::Stream, b"\0zero-send-permission", &credentials);
    let (client, _client_file) = endpoint(SocketType::Stream);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, server_file) = accepted(&listener);
    assert!(matches!(
        client.check_stream_send(true),
        Err(SocketError::AlreadyConnected)
    ));
    server.shutdown(0).unwrap();
    assert!(client.check_stream_send(false).is_ok());
    assert!(matches!(
        client.send_to_with_ancillary(b"", None, &mut None, &sender),
        Ok(0)
    ));
    assert!(matches!(
        client.send_to_with_ancillary(b"x", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
    drop(server_file);
    assert!(matches!(
        client.check_stream_send(false),
        Err(SocketError::BrokenPipe)
    ));
    assert!(matches!(
        client.send_to_with_ancillary(b"", None, &mut None, &sender),
        Err(socket::SocketSendError::Error(SocketError::BrokenPipe))
    ));
}

#[test]
fn seqpacket_protocol_preflight_consumes_actual_error_before_established_state() {
    let (credentials, _registration) = credential(72421);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) =
        listener(SocketType::SeqPacket, b"\0seq-send-preflight", &credentials);
    let (client, _client_file) = endpoint(SocketType::SeqPacket);
    assert!(matches!(
        client.check_seqpacket_send(),
        Err(SocketError::NotConnected)
    ));
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::SeqPacket)
    })
    .unwrap();
    let (_server, server_file) = accepted(&listener);
    assert!(client.check_seqpacket_send().is_ok());
    assert!(matches!(
        client.send_to_with_ancillary(b"unread", None, &mut None, &sender),
        Ok(6)
    ));
    drop(server_file);
    assert!(matches!(
        client.check_seqpacket_send(),
        Err(SocketError::ConnectionReset)
    ));
    assert!(client.check_seqpacket_send().is_ok());
    assert_eq!(client.take_error(), None);
}

#[test]
fn stream_actual_chunk_can_exceed_old_ring_and_refunds_only_at_final_byte() {
    let (credentials, _registration) = credential(72422);
    let sender = UnixSendCredentials {
        sender: credentials.clone(),
        claimed: None,
    };
    let (listener, _listener_file) = listener(
        SocketType::Stream,
        b"\0stream-native-chunk-window",
        &credentials,
    );
    let (client, _client_file) = endpoint(SocketType::Stream);
    UnixSocket::connect_oriented(&client, &listener, credentials, || {
        resources(SocketType::Stream)
    })
    .unwrap();
    let (server, _server_file) = accepted(&listener);
    let payload = vec![0x6c; 100_000];
    assert!(matches!(
        client.send_to_with_ancillary(&payload, None, &mut None, &sender),
        Ok(100_000)
    ));
    assert!(!client.poll_state().writable);
    let mut output = ipc::ReceiveBuffer::deferred(99_999);
    assert_eq!(
        socket::receive(&server, &mut output, false, false, true)
            .unwrap()
            .0,
        99_999
    );
    assert_eq!(output.initialized(), &payload[..99_999]);
    assert!(
        !client.poll_state().writable,
        "last queued byte still owns entire chunk allocation"
    );
    let mut byte = [0];
    assert_eq!(
        socket::receive(
            &server,
            &mut ipc::ReceiveBuffer::from_slice(&mut byte),
            false,
            false,
            true
        )
        .unwrap()
        .0,
        1
    );
    assert_eq!(byte, [0x6c]);
    assert!(client.poll_state().writable);
}
