#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 IPC namespace、queue、权限与事件 owner；调用者凭据/PID 是显式不可变输入。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/ipc_namespace_fixture.rs"]
mod ipc;
#[path = "../src/shm_backing_fixture.rs"]
mod memory;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

use alloc::sync::Arc;
use ipc::{
    IpcAccess, IpcError, IpcNamespace, MessageLimit, MessageQueueWait, MessageReceive,
    PreparedMessage,
};
use syscall_abi::ipc::*;

fn access() -> IpcAccess {
    namespace::boot();
    IpcAccess {
        authority: namespace::root_authority(),
        filesystem_gid: 0,
        groups: None,
    }
}
fn owner() -> Arc<IpcNamespace> {
    IpcNamespace::create(namespace::boot(), 0).unwrap()
}
fn pid() -> (Arc<namespace::PidIdentity>, namespace::PidRegistration) {
    namespace::boot();
    let view = namespace::initial_pid()
        .child(namespace::initial_user(), 0)
        .unwrap();
    let mut prepared =
        namespace::PreparedPid::try_new(999, view, &[], &namespace::root_authority()).unwrap();
    prepared.publish().unwrap();
    (prepared.identity(), prepared.take_registration())
}
fn send(
    queue: &ipc::MessageQueue,
    kind: i64,
    bytes: &[u8],
    access: &IpcAccess,
    pid: &Arc<namespace::PidIdentity>,
) {
    let mut message = PreparedMessage::new(kind, bytes.to_vec()).unwrap();
    assert!(
        queue
            .send(&mut message, access, pid.clone(), 12, None)
            .unwrap()
    );
}
#[test]
fn namespace_key_and_queue_contents_are_isolated_by_actual_owner() {
    let first = owner();
    let second = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = first
        .message_get(77, IPC_CREAT | 0o600, &access, 10)
        .unwrap();
    assert_eq!(
        second.message_get(77, 0, &access, 10),
        Err(IpcError::NotFound)
    );
    assert_eq!(
        first.message_get(77, IPC_CREAT | IPC_EXCL, &access, 10),
        Err(IpcError::Exists)
    );
    let other = second
        .message_get(77, IPC_CREAT | 0o600, &access, 10)
        .unwrap();
    send(&first.message_queue(id).unwrap(), 3, b"one", &access, &pid);
    assert!(
        second
            .message_queue(other)
            .unwrap()
            .receive(
                &MessageReceive::new(0, 0, 3, 8192).unwrap(),
                &access,
                pid.clone(),
                13,
                None
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        first
            .message_snapshot(id, false, false, &access)
            .unwrap()
            .bytes,
        3
    );
}
#[test]
fn type_selection_copy_and_e2big_preserve_linux_queue_semantics() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o600, &access, 1).unwrap();
    let queue = owner.message_queue(id).unwrap();
    send(&queue, 7, b"seven", &access, &pid);
    send(&queue, 2, b"two", &access, &pid);
    send(&queue, 3, b"three", &access, &pid);
    assert!(matches!(
        queue.receive(
            &MessageReceive::new(7, 0, 1, 8192).unwrap(),
            &access,
            pid.clone(),
            13,
            None
        ),
        Err(IpcError::TooBig)
    ));
    let message = queue
        .receive(
            &MessageReceive::new(1, MSG_COPY | IPC_NOWAIT, 8, 8192).unwrap(),
            &access,
            pid.clone(),
            13,
            None,
        )
        .unwrap()
        .unwrap();
    assert_eq!(message.kind, 2);
    assert_eq!(
        owner
            .message_snapshot(id, false, false, &access)
            .unwrap()
            .messages,
        3
    );
    assert_eq!(
        queue
            .receive(
                &MessageReceive::new(-7, 0, 8, 8192).unwrap(),
                &access,
                pid.clone(),
                13,
                None
            )
            .unwrap()
            .unwrap()
            .kind,
        2
    );
    assert_eq!(
        queue
            .receive(
                &MessageReceive::new(7, MSG_EXCEPT, 8, 8192).unwrap(),
                &access,
                pid.clone(),
                13,
                None
            )
            .unwrap()
            .unwrap()
            .kind,
        3
    );
    assert_eq!(
        queue
            .receive(
                &MessageReceive::new(0, MSG_NOERROR, 1, 8192).unwrap(),
                &access,
                pid.clone(),
                13,
                None
            )
            .unwrap()
            .unwrap()
            .bytes,
        b"seven"
    );
    let snapshot = owner.message_snapshot(id, false, false, &access).unwrap();
    assert_eq!(snapshot.bytes, 0);
    assert_eq!(snapshot.messages, 0);
    assert_eq!(snapshot.receive_time, 13);
    assert_eq!(
        snapshot.last_receiver.unwrap().number_in(&pid.namespace()),
        1
    );
}
#[test]
fn zero_length_messages_are_limited_and_rmid_wakes_exact_registered_waiter() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o600, &access, 1).unwrap();
    owner
        .message_update(id, 0, 0, 0o600, 2, &access, 2)
        .unwrap();
    let queue = owner.message_queue(id).unwrap();
    send(&queue, 1, b"", &access, &pid);
    send(&queue, 1, b"", &access, &pid);
    let mut message = PreparedMessage::new(1, Vec::new()).unwrap();
    let mut wait = MessageQueueWait::new(&queue).unwrap();
    assert!(
        !queue
            .send(&mut message, &access, pid.clone(), 3, Some(&mut wait))
            .unwrap()
    );
    owner.message_remove(id, &access).unwrap();
    assert_eq!(wait.wait(), crate::execution::sync::WaitResult::Woken);
    assert!(matches!(owner.message_queue(id), Err(IpcError::Invalid)));
    assert_eq!(
        queue.send(&mut message, &access, pid.clone(), 3, None),
        Err(IpcError::Removed)
    );
    let replacement = owner.message_get(0, 0o600, &access, 3).unwrap();
    assert_ne!(replacement, id);
}
#[test]
fn group_access_and_namespace_capabilities_do_not_grant_ancestor_access() {
    let owner = owner();
    let root = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o640, &root, 1).unwrap();
    owner
        .message_update(id, 20, 30, 0o640, 8, &root, 2)
        .unwrap();
    let queue = owner.message_queue(id).unwrap();
    let mut member = access();
    member.authority.effective_uid = 40;
    member.authority.effective_gid = 50;
    member.authority.effective_capabilities = 0;
    member.groups = Some(Arc::new(vec![30]));
    assert!(
        queue
            .receive(
                &MessageReceive::new(0, 0, 8, 8192).unwrap(),
                &member,
                pid.clone(),
                3,
                None
            )
            .unwrap()
            .is_none()
    );
    let mut message = PreparedMessage::new(1, vec![1]).unwrap();
    assert_eq!(
        queue.send(&mut message, &member, pid.clone(), 3, None),
        Err(IpcError::Access)
    );
    let child = namespace::UserNamespace::child(
        &root.authority,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let attacker = IpcAccess {
        filesystem_gid: 50,
        authority: namespace::NamespaceAuthority {
            namespace: child,
            effective_uid: 40,
            effective_gid: 50,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        },
        groups: None,
    };
    assert!(matches!(
        queue.receive(
            &MessageReceive::new(0, 0, 8, 8192).unwrap(),
            &attacker,
            pid.clone(),
            3,
            None
        ),
        Err(IpcError::Access)
    ));
    assert_eq!(
        owner.message_remove(id, &attacker),
        Err(IpcError::Permission)
    );
}
#[test]
fn namespace_retirement_marks_external_queue_pin_removed_and_reclaims_contents() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o600, &access, 1).unwrap();
    let queue = owner.message_queue(id).unwrap();
    send(&queue, 9, b"retired", &access, &pid);
    drop(owner);
    assert!(matches!(
        queue.receive(
            &MessageReceive::new(0, 0, 16, 8192).unwrap(),
            &access,
            pid.clone(),
            2,
            None
        ),
        Err(IpcError::Removed)
    ));
}
#[test]
fn sysctl_limits_are_namespace_local_and_require_mapped_root_euid() {
    let first = owner();
    let second = owner();
    let mut access = access();
    first
        .set_message_limit(MessageLimit::Queues, 0, &access.authority)
        .unwrap();
    assert_eq!(
        first.message_get(0, 0o600, &access, 1),
        Err(IpcError::NoSpace)
    );
    assert_eq!(second.message_limit(MessageLimit::Queues), 32000);
    access.authority.effective_uid = 30;
    assert_eq!(
        first.set_message_limit(MessageLimit::Queues, 1, &access.authority),
        Err(IpcError::Permission)
    );
    access.authority.effective_uid = 0;
    access.authority.effective_capabilities = 0;
    first
        .set_message_limit(MessageLimit::Queues, 1, &access.authority)
        .unwrap();
    assert_eq!(
        first.set_message_limit(MessageLimit::Queues, 32769, &access.authority),
        Err(IpcError::Invalid)
    );
}

#[test]
fn copy_preparation_capacity_and_noerror_follow_linux_copy_msg() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o600, &access, 1).unwrap();
    let queue = owner.message_queue(id).unwrap();
    send(&queue, 1, b"five!", &access, &pid);
    // msgmax 降低不删除已有消息；COPY 的预备 capacity 却由新的 msgmax 决定。
    for (capacity, maximum, flags, expected) in [
        (1, 8192, MSG_COPY | IPC_NOWAIT, IpcError::TooBig),
        (
            1,
            8192,
            MSG_COPY | IPC_NOWAIT | MSG_NOERROR,
            IpcError::Invalid,
        ),
        (8, 4, MSG_COPY | IPC_NOWAIT, IpcError::Invalid),
    ] {
        let request = MessageReceive::new(0, flags, capacity, maximum).unwrap();
        assert!(
            matches!(queue.receive(&request, &access, pid.clone(), 2, None), Err(error) if error == expected)
        );
    }
    assert_eq!(
        owner
            .message_snapshot(id, false, false, &access)
            .unwrap()
            .messages,
        1
    );
    for flags in [MSG_COPY, MSG_COPY | MSG_EXCEPT | IPC_NOWAIT] {
        assert!(matches!(
            MessageReceive::new(0, flags, 8, 8192),
            Err(IpcError::Invalid)
        ));
    }
    let request = MessageReceive::new(0, MSG_NOERROR, 1, 0).unwrap();
    assert!(
        queue
            .receive(&request, &access, pid, 2, None)
            .unwrap()
            .is_some()
    );
}

#[test]
fn pipelined_send_obeys_admission_order_and_is_not_revoked_by_rmid() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o600, &access, 1).unwrap();
    let queue = owner.message_queue(id).unwrap();
    let too_small = MessageReceive::new(7, 0, 1, 8192).unwrap();
    let request = MessageReceive::new(-7, 0, 8, 8192).unwrap();
    let mut small = MessageQueueWait::receiver(&queue, too_small, pid.clone()).unwrap();
    let mut first = MessageQueueWait::receiver(&queue, request, pid.clone()).unwrap();
    let mut second = MessageQueueWait::receiver(&queue, request, pid.clone()).unwrap();
    for (request, waiter) in [
        (&too_small, &mut small),
        (&request, &mut first),
        (&request, &mut second),
    ] {
        assert!(
            queue
                .receive(request, &access, pid.clone(), 2, Some(waiter))
                .unwrap()
                .is_none()
        );
    }
    send(&queue, 7, b"seven", &access, &pid);
    let stat = owner.message_snapshot(id, false, false, &access).unwrap();
    assert_eq!((stat.messages, stat.bytes), (0, 0));
    assert_eq!(stat.receive_time, 12);
    owner.message_remove(id, &access).unwrap();
    assert_eq!(small.wait(), crate::execution::sync::WaitResult::Woken);
    assert!(matches!(small.finish_wait(), Some(Err(IpcError::TooBig))));
    assert_eq!(first.wait(), crate::execution::sync::WaitResult::Woken);
    assert_eq!(first.finish_wait().unwrap().unwrap().bytes, b"seven");
    assert_eq!(second.wait(), crate::execution::sync::WaitResult::Woken);
    assert!(second.finish_wait().is_none());
    assert!(first.finish_wait().is_none());
}
#[test]
fn cancelled_receiver_cannot_consume_a_later_send() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o600, &access, 1).unwrap();
    let queue = owner.message_queue(id).unwrap();
    let request = MessageReceive::new(0, 0, 8, 8192).unwrap();
    let mut wait = MessageQueueWait::receiver(&queue, request, pid.clone()).unwrap();
    assert!(
        queue
            .receive(&request, &access, pid.clone(), 2, Some(&mut wait))
            .unwrap()
            .is_none()
    );
    assert!(wait.finish_wait().is_none());
    send(&queue, 1, b"next", &access, &pid);
    assert_eq!(
        owner
            .message_snapshot(id, false, false, &access)
            .unwrap()
            .messages,
        1
    );
    assert!(wait.finish_wait().is_none());
}

#[test]
fn ipc_group_dac_uses_fsgid_instead_of_effective_gid() {
    let owner = owner();
    let root = access();
    let (pid, _registration) = pid();
    let id = owner.message_get(0, 0o640, &root, 1).unwrap();
    owner
        .message_update(id, 20, 30, 0o640, 8, &root, 2)
        .unwrap();
    let queue = owner.message_queue(id).unwrap();
    let mut reader = access();
    reader.authority.effective_uid = 40;
    reader.authority.effective_gid = 30;
    reader.filesystem_gid = 50;
    reader.authority.effective_capabilities = 0;
    let request = MessageReceive::new(0, IPC_NOWAIT, 8, 8192).unwrap();
    assert!(matches!(
        queue.receive(&request, &reader, pid.clone(), 3, None),
        Err(IpcError::Access)
    ));
    reader.authority.effective_gid = 50;
    reader.filesystem_gid = 30;
    assert!(
        queue
            .receive(&request, &reader, pid, 3, None)
            .unwrap()
            .is_none()
    );
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
}
