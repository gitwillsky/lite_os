//! Proc symlink 的显示路径与 lookup target 由同一 inode owner 供给。
use super::{FileSystemError, ProcDirectory, ProcInode, ProcNode, Vec, decimal_name, vfs};

impl ProcInode {
    pub(super) fn readable_link(&self) -> Result<Vec<u8>, FileSystemError> {
        match self.node {
            ProcNode::NetLink => super::super::try_format_bytes(format_args!("self/net")),
            ProcNode::Mounts => super::super::try_format_bytes(format_args!("self/mounts")),
            ProcNode::SelfLink => {
                let pid = self
                    .source
                    .current_pid(&self.pid_namespace)
                    .ok_or(FileSystemError::NotFound)?;
                let mut stack = [0u8; 20];
                let name = decimal_name(pid, &mut stack);
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(name.len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                bytes.extend_from_slice(name);
                Ok(bytes)
            }
            ProcNode::NamespaceLink(tid, kind) => {
                let namespace = self
                    .source
                    .thread_namespace(tid, kind)?
                    .ok_or(FileSystemError::NotFound)?;
                let name =
                    core::str::from_utf8(namespace.kind().name()).expect("static namespace name");
                super::super::try_format_bytes(format_args!("{}:[{}]", name, namespace.inode()))
            }
            ProcNode::TaskFd(pid, fd) => self
                .source
                .thread_file_descriptors(pid)?
                .and_then(|entries| entries.into_iter().find(|entry| entry.fd == fd))
                .map(|entry| {
                    vfs().opened_path(
                        &entry.target,
                        &self
                            .source
                            .current_root()
                            .ok_or(FileSystemError::NotFound)?,
                    )
                })
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::TaskExe(pid) => self
                .source
                .thread_executable(pid)?
                .ok_or(FileSystemError::NotFound)
                .and_then(|opened| {
                    vfs().opened_path(
                        &opened,
                        &self
                            .source
                            .current_root()
                            .ok_or(FileSystemError::NotFound)?,
                    )
                }),
            ProcNode::ProcessRoot(pid)
            | ProcNode::ProcessCwd(pid)
            | ProcNode::ThreadRoot(pid)
            | ProcNode::ThreadCwd(pid) => self
                .source
                .process_directory(
                    pid,
                    if matches!(
                        self.node,
                        ProcNode::ProcessRoot(_) | ProcNode::ThreadRoot(_)
                    ) {
                        ProcDirectory::Root
                    } else {
                        ProcDirectory::Cwd
                    },
                )?
                .ok_or(FileSystemError::NotFound)
                .and_then(|opened| {
                    vfs().opened_path(
                        &opened,
                        &self
                            .source
                            .current_root()
                            .ok_or(FileSystemError::NotFound)?,
                    )
                }),
            _ => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(super) fn resolved_link(&self) -> Result<super::super::LinkTarget, FileSystemError> {
        use super::super::LinkTarget;
        let opened = match self.node {
            ProcNode::NamespaceLink(tid, kind) => {
                let namespace = self
                    .source
                    .thread_namespace(tid, kind)?
                    .ok_or(FileSystemError::NotFound)?;
                return super::super::namespace_opened(namespace).map(LinkTarget::Opened);
            }
            ProcNode::TaskFd(pid, fd) => {
                let entry = self
                    .source
                    .thread_file_descriptors(pid)?
                    .and_then(|entries| entries.into_iter().find(|entry| entry.fd == fd))
                    .ok_or(FileSystemError::NotFound)?;
                return Ok(LinkTarget::Opened(entry.target));
            }
            ProcNode::TaskExe(pid) => self.source.thread_executable(pid)?,
            ProcNode::ProcessRoot(pid) | ProcNode::ThreadRoot(pid) => {
                self.source.process_directory(pid, ProcDirectory::Root)?
            }
            ProcNode::ProcessCwd(pid) | ProcNode::ThreadCwd(pid) => {
                self.source.process_directory(pid, ProcDirectory::Cwd)?
            }
            _ => return self.readable_link().map(LinkTarget::Text),
        };
        opened
            .map(LinkTarget::Opened)
            .ok_or(FileSystemError::NotFound)
    }
}
