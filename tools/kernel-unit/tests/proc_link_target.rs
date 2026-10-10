#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production ProcInode link operations; the upper source and path projector are explicit seams.
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/storage/fs"]
mod fs {
    use alloc::{sync::Arc, vec::Vec};
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        OutOfMemory,
        AccessDenied,
        NotFound,
        InvalidOperation,
    }
    pub(crate) struct OpenedFile;
    pub(crate) enum LinkTarget {
        Text(Vec<u8>),
        Opened(Arc<OpenedFile>),
    }
    fn try_format_bytes(args: core::fmt::Arguments<'_>) -> Result<Vec<u8>, FileSystemError> {
        Ok(alloc::fmt::format(args).into_bytes())
    }
    fn namespace_opened(
        _: crate::namespace::NamespaceReference,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        unreachable!("namespace inode producer seam")
    }
    mod procfs {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        mod link;
        #[derive(Clone, Copy)]
        enum ProcDirectory {
            Root,
            Cwd,
        }
        struct ProcFileDescriptorSnapshot {
            fd: usize,
            target: Arc<OpenedFile>,
        }
        #[derive(Clone, Copy)]
        enum ProcNode {
            Mounts,
            NetLink,
            NamespaceLink(usize, crate::namespace::NamespaceSelector),
            SelfLink,
            TaskFd(usize, usize),
            TaskExe(usize),
            ProcessRoot(usize),
            ProcessCwd(usize),
            ThreadRoot(usize),
            ThreadCwd(usize),
            Other,
        }
        #[derive(Clone, Copy)]
        enum Behavior {
            Opened,
            Missing,
            Error(FileSystemError),
        }
        struct Source {
            behavior: Behavior,
            calls: AtomicUsize,
            opened: Arc<OpenedFile>,
        }
        impl Source {
            fn thread_namespace(
                &self,
                _: usize,
                _: crate::namespace::NamespaceSelector,
            ) -> Result<Option<crate::namespace::NamespaceReference>, FileSystemError> {
                unreachable!("namespace projection seam")
            }
            fn current_pid(&self, _: &crate::namespace::PidNamespace) -> Option<usize> {
                Some(42)
            }
            fn current_root(&self) -> Option<Arc<OpenedFile>> {
                panic!("lookup must not request viewer projection")
            }
            fn thread_file_descriptors(
                &self,
                _: usize,
            ) -> Result<Option<Vec<ProcFileDescriptorSnapshot>>, FileSystemError> {
                assert_eq!(
                    self.calls.fetch_add(1, Ordering::Relaxed),
                    0,
                    "target must not be queried twice"
                );
                match self.behavior {
                    Behavior::Error(error) => Err(error),
                    Behavior::Missing => Ok(None),
                    Behavior::Opened => Ok(Some(vec![ProcFileDescriptorSnapshot {
                        fd: 7,
                        target: self.opened.clone(),
                    }])),
                }
            }
            fn thread_executable(
                &self,
                _: usize,
            ) -> Result<Option<Arc<OpenedFile>>, FileSystemError> {
                self.process_directory(0, ProcDirectory::Root)
            }
            fn process_directory(
                &self,
                _: usize,
                _: ProcDirectory,
            ) -> Result<Option<Arc<OpenedFile>>, FileSystemError> {
                match self.behavior {
                    Behavior::Error(error) => Err(error),
                    Behavior::Missing => Ok(None),
                    _ => Ok(Some(self.opened.clone())),
                }
            }
        }
        struct ProcInode {
            pid_namespace: Arc<crate::namespace::PidNamespace>,
            node: ProcNode,
            source: Source,
        }
        struct Projector;
        fn vfs() -> Projector {
            Projector
        }
        impl Projector {
            fn opened_path(
                &self,
                _: &Arc<OpenedFile>,
                _: &Arc<OpenedFile>,
            ) -> Result<Vec<u8>, FileSystemError> {
                panic!("lookup must not recursively enter namespace projection")
            }
        }
        fn decimal_name(value: usize, output: &mut [u8; 20]) -> &[u8] {
            let mut value = value;
            let mut start = output.len();
            loop {
                start -= 1;
                output[start] = b'0' + (value % 10) as u8;
                value /= 10;
                if value == 0 {
                    return &output[start..];
                }
            }
        }
        fn inode(node: ProcNode, behavior: Behavior) -> ProcInode {
            crate::namespace::boot();
            ProcInode {
                pid_namespace: crate::namespace::initial_pid(),
                node,
                source: Source {
                    behavior,
                    calls: AtomicUsize::new(0),
                    opened: Arc::new(OpenedFile),
                },
            }
        }
        #[test]
        fn actual_proc_fd_target_errors_and_disappearance_do_not_fall_back() {
            for error in [FileSystemError::OutOfMemory, FileSystemError::AccessDenied] {
                let link = inode(ProcNode::TaskFd(42, 7), Behavior::Error(error));
                assert!(matches!(link.resolved_link(), Err(actual) if actual == error));
                assert_eq!(link.source.calls.load(Ordering::Relaxed), 1);
            }
            let missing = inode(ProcNode::TaskFd(42, 7), Behavior::Missing);
            assert!(matches!(
                missing.resolved_link(),
                Err(FileSystemError::NotFound)
            ));
            let reused = inode(ProcNode::TaskFd(42, 8), Behavior::Opened);
            assert!(matches!(
                reused.resolved_link(),
                Err(FileSystemError::NotFound)
            ));
        }
        #[test]
        fn actual_proc_fd_freezes_opened_identity_without_projection() {
            let opened = inode(ProcNode::TaskFd(42, 7), Behavior::Opened);
            assert!(
                matches!(opened.resolved_link(), Ok(LinkTarget::Opened(target)) if Arc::ptr_eq(&target, &opened.source.opened))
            );
            let own = inode(ProcNode::SelfLink, Behavior::Missing);
            assert!(matches!(own.resolved_link(), Ok(LinkTarget::Text(target)) if target == b"42"));
        }
        #[test]
        fn actual_proc_directory_and_executable_targets_preserve_errors() {
            for node in [
                ProcNode::ProcessRoot(42),
                ProcNode::ProcessCwd(42),
                ProcNode::ThreadRoot(43),
                ProcNode::ThreadCwd(43),
                ProcNode::TaskExe(42),
            ] {
                assert!(matches!(
                    inode(node, Behavior::Missing).resolved_link(),
                    Err(FileSystemError::NotFound)
                ));
                assert!(matches!(
                    inode(node, Behavior::Error(FileSystemError::OutOfMemory)).resolved_link(),
                    Err(FileSystemError::OutOfMemory)
                ));
                let live = inode(node, Behavior::Opened);
                assert!(
                    matches!(live.resolved_link(), Ok(LinkTarget::Opened(target)) if Arc::ptr_eq(&target, &live.source.opened))
                );
            }
        }
    }
}
