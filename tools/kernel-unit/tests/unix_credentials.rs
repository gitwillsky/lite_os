#![feature(allocator_ext)]
#![allow(dead_code)]
//! actual native ucred codec/权限/namespace PID observer；calling Task 只提供冻结 owner 输入。
extern crate alloc;
use alloc::sync::Arc;
use syscall_abi::errno;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/net/credentials.rs"]
mod net;

#[path = "../../../kernel/src/syscall"]
mod syscall {
    pub(crate) mod socket {
        use syscall_abi::errno;
        pub(crate) mod credentials;
        pub(crate) fn prepare(
            task: &crate::execution::task::TaskControlBlock,
            bytes: &[u8],
        ) -> Result<crate::net::UnixCredentials, isize> {
            credentials::prepare(task, bytes)
        }
        pub(crate) fn encode(
            task: &crate::execution::task::TaskControlBlock,
            value: Option<&crate::net::UnixCredentials>,
        ) -> [u8; 12] {
            credentials::encode(task, value)
        }
        pub(crate) fn sender(
            task: &crate::execution::task::TaskControlBlock,
        ) -> crate::net::UnixSendCredentials {
            credentials::sender(task)
        }
    }
}
use syscall::socket as credentials;
fn bytes(pid: i32, uid: u32, gid: u32) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[..4].copy_from_slice(&pid.to_ne_bytes());
    bytes[4..8].copy_from_slice(&uid.to_ne_bytes());
    bytes[8..].copy_from_slice(&gid.to_ne_bytes());
    bytes
}
fn caller(
    key: usize,
) -> (
    crate::execution::task::TaskControlBlock,
    namespace::PidRegistration,
) {
    namespace::boot();
    let view = namespace::initial_pid();
    let mut pid = namespace::PreparedPid::try_new(
        key,
        view.clone(),
        &[],
        &crate::namespace::root_authority(),
    )
    .unwrap();
    pid.publish().unwrap();
    let registration = pid.take_registration();
    (
        crate::execution::task::TaskControlBlock {
            authority: namespace::NamespaceAuthority {
                namespace: namespace::initial_user(),
                effective_uid: 1002,
                effective_gid: 2002,
                effective_capabilities: 0,
            },
            uids: [1001, 1002, 1003],
            gids: [2001, 2002, 2003],
            tgid: pid.identity(),
            namespace: view,
        },
        registration,
    )
}
#[test]
fn actual_owner_ids_mapping_and_all_native_real_effective_saved_claims_are_validated() {
    let (actor, _registration) = caller(72100);
    let pid = actor.tgid.number_in(&actor.namespace) as i32;
    assert_eq!(actor.message_identity().ids(false), [1001, 2001]);
    assert_eq!(actor.message_identity().ids(true), [1002, 2002]);
    for uid in actor.uids {
        for gid in actor.gids {
            let prepared = credentials::prepare(&actor, &bytes(pid, uid, gid)).unwrap();
            assert!(Arc::ptr_eq(&prepared.pid, &actor.tgid));
            assert_eq!([prepared.uid, prepared.gid], [uid, gid]);
        }
    }
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid,1004,2001)), Err(error) if error == -errno::EPERM)
    );
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid,u32::MAX,2001)), Err(error) if error == -errno::EINVAL)
    );
    assert!(matches!(credentials::prepare(&actor, &[0;11]), Err(error) if error == -errno::EINVAL));
}
#[test]
fn actual_namespace_pid_lookup_retains_group_only_identity_and_rejects_nonexistent_pid_after_permission()
 {
    let (mut actor, _registration) = caller(72101);
    let (target, target_registration) = caller(72102);
    let pid = target.tgid.number_in(&actor.namespace) as i32;
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid,1001,2001)), Err(error) if error == -errno::EPERM)
    );
    actor.authority.effective_capabilities = syscall_abi::capability::Capability::SYS_ADMIN.mask();
    let group = target_registration.clone_in(namespace::PidLinkSlot::try_new().unwrap());
    drop(target_registration);
    let received = credentials::prepare(&actor, &bytes(pid, 1001, 2001)).unwrap();
    assert!(Arc::ptr_eq(&received.pid, &target.tgid));
    drop(group);
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid,1001,2001)), Err(error) if error == -errno::ESRCH)
    );
    assert_eq!(received.pid.key(), target.tgid.key());
    for invalid in [-1, 0, i32::MAX] {
        assert!(
            matches!(credentials::prepare(&actor, &bytes(invalid,1001,2001)), Err(error) if error == -errno::ESRCH)
        );
    }
}

#[test]
fn namespace_scoped_capability_cannot_spoof_pid_owned_by_ancestor_user_namespace() {
    let (mut actor, _registration) = caller(72103);
    let root = actor.authority.namespace.clone();
    let authority = namespace::root_authority();
    let child = namespace::UserNamespace::child(
        &authority,
        true,
        namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    child
        .publish_map(true, b"0 1001 3\n", &authority, &authority)
        .unwrap();
    child
        .publish_map(false, b"0 2001 3\n", &authority, &authority)
        .unwrap();
    actor.authority.namespace = child.clone();
    actor.authority.effective_capabilities = syscall_abi::capability::VALID_MASK;
    let pid = actor.tgid.number_in(&actor.namespace) as i32;
    let allowed = credentials::prepare(&actor, &bytes(pid, 0, 0)).unwrap();
    assert_eq!([allowed.uid, allowed.gid], [1001, 2001]);
    let output = credentials::encode(&actor, Some(&allowed));
    assert_eq!(&output[4..8], &0u32.to_ne_bytes());
    assert_eq!(&output[8..], &0u32.to_ne_bytes());
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid+1,0,0)), Err(error) if error == -errno::EPERM)
    );
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid+1,3,0)), Err(error) if error == -errno::EINVAL)
    );
    assert!(
        actor
            .message_identity()
            .validate_claim(0, 0, false, &child)
            .is_ok()
    );
    assert!(matches!(
        actor.message_identity().validate_claim(0, 0, false, &root),
        Err(crate::execution::task::MessageIdentityError::Permission)
    ));
}

#[test]
fn setuid_and_setgid_claim_exemptions_require_each_actual_sender_capability() {
    use syscall_abi::capability::Capability;
    let (mut actor, _registration) = caller(72104);
    let pid = actor.tgid.number_in(&actor.namespace) as i32;
    actor.authority.effective_capabilities = Capability::SETUID.mask();
    let prepared = credentials::prepare(&actor, &bytes(pid, 5000, 2001)).unwrap();
    assert_eq!(prepared.uid, 5000);
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid,5000,6000)), Err(error) if error == -errno::EPERM)
    );
    actor.authority.effective_capabilities |= Capability::SETGID.mask();
    let prepared = credentials::prepare(&actor, &bytes(pid, 5000, 6000)).unwrap();
    assert_eq!([prepared.uid, prepared.gid], [5000, 6000]);
    assert!(
        matches!(credentials::prepare(&actor, &bytes(pid,u32::MAX,6000)), Err(error) if error == -errno::EINVAL)
    );
}

#[test]
fn receive_codec_projects_actual_namespaces_and_native_missing_credentials() {
    let (actor, _registration) = caller(72306);
    let native = credentials::encode(&actor, None);
    assert_eq!(&native[..4], &0i32.to_ne_bytes());
    assert_eq!(&native[4..8], &65534u32.to_ne_bytes());
    assert_eq!(&native[8..], &65534u32.to_ne_bytes());
    let actual = credentials::sender(&actor);
    let native = credentials::encode(&actor, Some(&actual.sender));
    assert_eq!(
        &native[..4],
        &(actor.tgid.number_in(&actor.namespace) as i32).to_ne_bytes()
    );
    assert_eq!(&native[4..8], &1001u32.to_ne_bytes());
    assert_eq!(&native[8..], &2001u32.to_ne_bytes());
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../../../kernel/src/execution/task/model"]
    pub(crate) mod task {
        use crate::namespace::{NamespaceAuthority, PidIdentity, PidNamespace};
        use alloc::sync::Arc;
        mod message_identity;
        pub(crate) use message_identity::MessageIdentityError;
        pub(crate) struct TaskControlBlock {
            pub(crate) authority: NamespaceAuthority,
            pub(crate) uids: [u32; 3],
            pub(crate) gids: [u32; 3],
            pub(crate) tgid: Arc<PidIdentity>,
            pub(crate) namespace: Arc<PidNamespace>,
        }
        impl TaskControlBlock {
            pub(crate) fn message_identity(&self) -> message_identity::MessageIdentity {
                message_identity::MessageIdentity::new(self.authority.clone(), self.uids, self.gids)
            }
            pub(crate) fn user_namespace(&self) -> Arc<crate::namespace::UserNamespace> {
                self.authority.namespace.clone()
            }
            pub(crate) fn pid_namespace(&self) -> Arc<PidNamespace> {
                self.namespace.clone()
            }
            pub(crate) fn tgid_identity(&self) -> &Arc<PidIdentity> {
                &self.tgid
            }
        }
    }
}
