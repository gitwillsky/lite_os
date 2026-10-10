#![feature(allocator_ext)]
#![allow(dead_code)]
extern crate alloc;
#[path = "../src/shm_backing_fixture.rs"]
mod memory;
#[path = "../src/socket_core_fixture.rs"]
mod socket;
#[path = "../src/sync.rs"]
mod sync;

#[path = "../src/ipc_namespace_fixture.rs"]
mod ipc;

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/storage/fs"]
mod fs {
    #[path = "inotify/limits.rs"]
    mod inotify_limits;
    pub(crate) use inotify_limits::InotifyLimit;
    pub(crate) enum MountTableKind {
        Mounts,
        Info,
        Stats,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InodeType {
    File,
    Directory,
    SymLink,
}
#[path = "../../../kernel/src/storage/fs/procfs/node.rs"]
mod node;
#[test]
fn actual_proc_metadata_preserves_fixed_linux_link_sizes() {
    use node::ProcNode;
    for item in [ProcNode::TaskFd(4, 7), ProcNode::TaskFd(9, 1023)] {
        assert_eq!(item.metadata_size(), 64);
        assert_eq!(item.kind(), InodeType::SymLink);
    }
    for item in [
        ProcNode::ProcessRoot(4),
        ProcNode::ProcessCwd(4),
        ProcNode::ThreadRoot(5),
        ProcNode::ThreadCwd(5),
        ProcNode::TaskExe(4),
    ] {
        assert_eq!(item.metadata_size(), 0);
        assert_eq!(item.kind(), InodeType::SymLink);
    }
    assert_eq!(ProcNode::Mounts.metadata_size(), 0);
    assert_eq!(ProcNode::ProcessStatus(4).metadata_size(), 0);
    // NET sysctl的UID/GID/mode归actual scoped inode，不借跨namespace的registered entry属性。
    for node in [
        ProcNode::NetSysctlDir,
        ProcNode::CoreNetSysctlDir,
        ProcNode::OptmemLimit,
    ] {
        assert!(node.generic_index().is_none());
    }
    for resource in socket::CoreBufferLimit::ALL {
        assert!(
            ProcNode::CoreBufferLimit(resource)
                .generic_index()
                .is_none()
        );
    }
}
