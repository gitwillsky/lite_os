//! proc 的实际 per-open 操作；f_cred 与 map target 只在成功打开前捕获一次。
use super::{FileSystemError, ProcInode, ProcNode, ProcText};
use crate::fs::chardev::{DeviceError, DeviceFile, PositionedFile, UserInput, UserOutput};
use crate::namespace::{NamespaceAuthority, NamespaceError, UserNamespace, UserNamespaceMap};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt::Write;
use syscall_abi::errno;

// OWNER: map target 与 opener authority 都不借 Task；SCM_RIGHTS/dup/fork 保持同一 f_cred。
// Snapshot 分支仍只渲染 live source，viewer namespace 是打开时的 immutable credential projection。
enum ProcOpenFile {
    Network {
        source: Arc<dyn super::ProcSource>,
        target: Arc<crate::net::NetNamespace>,
        table: super::networking::NetworkTable,
    },
    TimeOffsets {
        inode: Arc<ProcInode>,
        tid: usize,
        opener: NamespaceAuthority,
    },
    Mounts {
        target: Arc<crate::fs::MountNamespace>,
        root: Arc<crate::fs::OpenedFile>,
        kind: crate::fs::MountTableKind,
        event: core::sync::atomic::AtomicU64,
    },
    Snapshot {
        inode: Arc<ProcInode>,
        viewer: Arc<UserNamespace>,
    },
    Limit {
        target: Arc<UserNamespace>,
        resource: crate::namespace::UserResource,
    },
    MountLimit,
    InotifyLimit(crate::fs::InotifyLimit),
    CoreBufferLimit {
        target: Arc<crate::net::NetNamespace>,
        resource: crate::net::CoreBufferLimit,
    },
    OptmemLimit(Arc<crate::net::NetNamespace>),
    IpcTable {
        target: Arc<crate::ipc::IpcNamespace>,
        table: crate::ipc::IpcTable,
        viewer: Arc<UserNamespace>,
        pid: Arc<crate::namespace::PidNamespace>,
        // OWNER: 同一 actual open 的 seq cursor；reader TaskMutex 串行 copy，IPC registry 短 guard 不跨 codec/user-copy。
        reader: Box<crate::execution::sync::TaskMutex<super::ipc_description::IpcReadCursor>>,
    },
    IpcLimit {
        target: Arc<crate::ipc::IpcNamespace>,
        resource: crate::ipc::IpcLimit,
    },
    PidLimit {
        target: Arc<crate::namespace::PidNamespace>,
    },
    Map {
        target: Arc<UserNamespace>,
        kind: UserNamespaceMap,
        opener: NamespaceAuthority,
    },
}
pub(super) fn open(
    inode: &ProcInode,
    flags: u32,
    opener: &NamespaceAuthority,
) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
    let file = if let Some((tid, kind)) = inode.node.mount_table() {
        let (target, root) = inode
            .source
            .thread_mount_snapshot(tid)
            .ok_or(FileSystemError::NotFound)?;
        let event = core::sync::atomic::AtomicU64::new(target.event());
        ProcOpenFile::Mounts {
            target,
            root,
            kind,
            event,
        }
    } else if let Some(table) = super::networking::NetworkTable::from_node(inode.node) {
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY {
            return Err(FileSystemError::AccessDenied);
        }
        // seq_open_net 对实际 child NET 提升强 pin；不能从 target Task 或本次 caller 重解析。
        let target = inode
            .net_namespace
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(FileSystemError::NoDevice)?;
        ProcOpenFile::Network {
            source: inode.source.clone(),
            target,
            table,
        }
    } else if let ProcNode::TimeOffsets(tid) = inode.node {
        ProcOpenFile::TimeOffsets {
            inode: inode.shared(),
            tid,
            opener: opener.clone(),
        }
    } else if let ProcNode::UserMap(tid, kind) = inode.node {
        let credentials = inode
            .source
            .thread_credentials(tid)?
            .ok_or(FileSystemError::NotFound)?;
        if kind == UserNamespaceMap::Setgroups
            && flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY
            && !opener.capable(
                &credentials.user_namespace,
                syscall_abi::capability::Capability::SYS_ADMIN,
            )
        {
            return Err(FileSystemError::AccessDenied);
        }
        ProcOpenFile::Map {
            target: credentials.user_namespace,
            kind,
            opener: opener.clone(),
        }
    } else if let ProcNode::PidLimit = inode.node {
        let target = inode
            .sysctl_pid_namespace
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(FileSystemError::NotFound)?;
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY && !target.may_set_limit(opener) {
            return Err(FileSystemError::AccessDenied);
        }
        ProcOpenFile::PidLimit { target }
    } else if let ProcNode::IpcTable(table) = inode.node {
        // Linux sysvipc_proc_open：固定实际 caller IPC/PID ns，seq_user_ns 固定 f_cred user ns。
        ProcOpenFile::IpcTable {
            target: inode.source.current_ipc_namespace()?,
            table,
            viewer: opener.namespace.clone(),
            pid: inode.source.current_pid_namespace(),
            reader: Box::try_new(crate::execution::sync::TaskMutex::new(
                super::ipc_description::IpcReadCursor::new(),
            ))
            .map_err(|_| FileSystemError::OutOfMemory)?,
        }
    } else if let ProcNode::IpcLimit(resource) = inode.node {
        let target = inode
            .sysctl_ipc_namespace
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(FileSystemError::NotFound)?;
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY && !target.may_set_limits(opener) {
            return Err(FileSystemError::AccessDenied);
        }
        ProcOpenFile::IpcLimit { target, resource }
    } else if let ProcNode::InotifyLimit(resource) = inode.node {
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY {
            super::sysctl_permissions::require(opener.effective_uid, 2)?;
        }
        ProcOpenFile::InotifyLimit(resource)
    } else if let ProcNode::CoreBufferLimit(resource) = inode.node {
        let target = inode
            .net_namespace
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(FileSystemError::NotFound)?;
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY
            && !may_set_core_buffers(&target, opener)
        {
            return Err(FileSystemError::AccessDenied);
        }
        ProcOpenFile::CoreBufferLimit { target, resource }
    } else if let ProcNode::OptmemLimit = inode.node {
        let target = inode
            .net_namespace
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(FileSystemError::NotFound)?;
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY && !target.may_set_limits(opener) {
            return Err(FileSystemError::AccessDenied);
        }
        ProcOpenFile::OptmemLimit(target)
    } else if let ProcNode::MountLimit = inode.node {
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY {
            super::sysctl_permissions::require(opener.effective_uid, 2)?;
        }
        ProcOpenFile::MountLimit
    } else if let ProcNode::UserResourceLimit(resource) = inode.node {
        let target = inode
            .user_namespace
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(FileSystemError::NotFound)?;
        if flags & crate::fs::O_ACCMODE != crate::fs::O_RDONLY
            && !opener.capable(&target, syscall_abi::capability::Capability::SYS_RESOURCE)
        {
            return Err(FileSystemError::AccessDenied);
        }
        ProcOpenFile::Limit { target, resource }
    } else {
        ProcOpenFile::Snapshot {
            inode: inode.shared(),
            viewer: opener.namespace.clone(),
        }
    };
    Arc::try_new(file)
        .map(|file| file as Arc<dyn DeviceFile>)
        .map_err(|_| FileSystemError::OutOfMemory)
}
fn namespace_error(error: NamespaceError) -> DeviceError {
    DeviceError::Errno(match error {
        NamespaceError::Invalid => errno::EINVAL,
        NamespaceError::Permission => errno::EPERM,
        NamespaceError::OutOfMemory => errno::ENOMEM,
        NamespaceError::Multithreaded => errno::EUSERS,
        NamespaceError::NestingLimit | NamespaceError::QuotaLimit => errno::ENOSPC,
    })
}
fn may_set_core_buffers(target: &crate::net::NetNamespace, authority: &NamespaceAuthority) -> bool {
    target.is_initial() && target.may_set_limits(authority)
}
fn ipc_error(error: crate::ipc::IpcError) -> DeviceError {
    DeviceError::Errno(match error {
        crate::ipc::IpcError::Permission => errno::EPERM,
        crate::ipc::IpcError::Range => errno::ERANGE,
        _ => errno::EINVAL,
    })
}
fn filesystem_error(error: FileSystemError) -> DeviceError {
    DeviceError::Errno(match error {
        FileSystemError::OutOfMemory => errno::ENOMEM,
        FileSystemError::NotFound => errno::ENOENT,
        FileSystemError::NoDevice => errno::ENXIO,
        FileSystemError::AccessDenied => errno::EACCES,
        FileSystemError::PermissionDenied => errno::EPERM,
        _ => errno::EIO,
    })
}
impl ProcOpenFile {
    fn contents(&self) -> Result<Vec<u8>, FileSystemError> {
        if let Self::Network {
            source,
            target,
            table,
        } = self
        {
            return match table {
                super::networking::NetworkTable::Devices => {
                    super::networking::contents(*table, &source.network_snapshot(target)?, &[])
                }
                super::networking::NetworkTable::Routes => {
                    super::networking::contents(*table, &[], &source.route_snapshot(target)?)
                }
            };
        }
        if let Self::TimeOffsets { inode, tid, .. } = self {
            return super::time_offsets::contents(inode.source.as_ref(), *tid);
        }
        if let Self::PidLimit { target } = self {
            return super::super::try_format_bytes(format_args!("{}\n", target.limit()));
        }
        if let Self::CoreBufferLimit { resource, .. } = self {
            return super::super::try_format_bytes(format_args!("{}\n", resource.value()));
        }
        if let Self::OptmemLimit(target) = self {
            return super::super::try_format_bytes(format_args!("{}\n", target.optmem_max()));
        }
        if let Self::IpcLimit { target, resource } = self {
            return match target.sysctl_value(*resource) {
                crate::ipc::IpcLimitValue::Message(value) => {
                    super::super::try_format_bytes(format_args!("{}\n", value))
                }
                crate::ipc::IpcLimitValue::SharedMemory(value) => {
                    super::super::try_format_bytes(format_args!("{}\n", value))
                }
                crate::ipc::IpcLimitValue::Semaphores([set, total, operations, arrays]) => {
                    super::super::try_format_bytes(format_args!(
                        "{}\t{}\t{}\t{}\n",
                        set, total, operations, arrays
                    ))
                }
            };
        }
        if let Self::InotifyLimit(resource) = self {
            return super::super::try_format_bytes(format_args!("{}\n", resource.value()));
        }
        if let Self::MountLimit = self {
            // 固定0644对所有caller授予read，且proc_sys_setattr禁止改变mode。
            return super::super::try_format_bytes(format_args!("{}\n", crate::fs::mount_limit()));
        }
        if let Self::Mounts {
            target, root, kind, ..
        } = self
        {
            return crate::fs::vfs().mount_table(target, root, *kind);
        }
        if let Self::Limit { target, resource } = self {
            return super::super::try_format_bytes(format_args!(
                "{}\n",
                target.resource_limit(*resource)
            ));
        }
        let Self::Map {
            target,
            kind,
            opener,
        } = self
        else {
            let Self::Snapshot { inode, viewer } = self else {
                unreachable!()
            };
            return inode.file_contents(viewer);
        };
        let mut output = ProcText::new();
        if *kind == UserNamespaceMap::Setgroups {
            output
                .write_str(if target.setgroups_allowed() {
                    "allow\n"
                } else {
                    "deny\n"
                })
                .map_err(|_| FileSystemError::OutOfMemory)?;
        } else if target.is_initial() {
            writeln!(
                output,
                "{:10} {:10} {:10}",
                0,
                opener
                    .namespace
                    .map_kernel_id(*kind == UserNamespaceMap::Uid, 0)
                    .unwrap_or(u32::MAX),
                u32::MAX
            )
            .map_err(|_| FileSystemError::OutOfMemory)?;
        } else if let Some(map) = target.mapping_extents(*kind == UserNamespaceMap::Uid) {
            let viewer = if Arc::ptr_eq(target, &opener.namespace) {
                target
                    .parent()
                    .expect("noninitial map namespace requires parent")
            } else {
                opener.namespace.clone()
            };
            for extent in map.extents() {
                let lower = viewer
                    .map_kernel_id(*kind == UserNamespaceMap::Uid, extent.lower_first)
                    .unwrap_or(u32::MAX);
                writeln!(
                    output,
                    "{:10} {:10} {:10}",
                    extent.first, lower, extent.count
                )
                .map_err(|_| FileSystemError::OutOfMemory)?;
            }
        }
        Ok(output.finish())
    }
}
impl DeviceFile for ProcOpenFile {
    fn positioned(&self) -> Option<&dyn PositionedFile> {
        Some(self)
    }
    fn poll(&self, events: i16) -> i16 {
        if let Self::Mounts {
            target,
            kind,
            event,
            ..
        } = self
        {
            if *kind == crate::fs::MountTableKind::Stats {
                return events & 5;
            }
            let current = target.event();
            let changed = event.swap(current, core::sync::atomic::Ordering::AcqRel) != current;
            return (events & (1 | 0x40)) | if changed { (events & 2) | 8 } else { 0 };
        }
        events & 5
    }
    fn wait_sources(&self, events: i16) -> crate::fs::chardev::DeviceWaitSources {
        match self {
            Self::Mounts { target, kind, .. } if *kind != crate::fs::MountTableKind::Stats => {
                crate::fs::chardev::DeviceWaitSources::pipe(target.notification_pipe(), events | 8)
            }
            _ => crate::fs::chardev::DeviceWaitSources::new(),
        }
    }
    fn readiness_generation(&self) -> u64 {
        match self {
            Self::Mounts { target, .. } => target.event(),
            _ => 0,
        }
    }
    fn prepare_wait(&self, events: i16) -> Option<Arc<crate::ipc::Pipe>> {
        let Self::Mounts {
            target,
            kind,
            event,
            ..
        } = self
        else {
            return None;
        };
        if *kind == crate::fs::MountTableKind::Stats || events & (1 | 0x40) != 0 {
            return None;
        }
        target.drain_notification();
        (event.load(core::sync::atomic::Ordering::Acquire) == target.event())
            .then(|| target.notification_pipe())
    }
}
impl PositionedFile for ProcOpenFile {
    fn read_at(&self, output: &mut dyn UserOutput, offset: u64) -> Result<(), DeviceError> {
        if output.remaining() == 0
            || offset != 0
                && matches!(
                    self,
                    Self::Limit { .. }
                        | Self::MountLimit
                        | Self::InotifyLimit(_)
                        | Self::CoreBufferLimit { .. }
                        | Self::OptmemLimit(_)
                        | Self::PidLimit { .. }
                )
        {
            return Ok(());
        }
        if let Self::IpcTable {
            target,
            table,
            viewer,
            pid,
            reader,
        } = self
        {
            return reader
                .lock()
                .map_err(|_| DeviceError::Errno(errno::ENOMEM))?
                .read(output, offset, target, *table, viewer, pid);
        }
        let contents = self.contents().map_err(|error| {
            if matches!(self, Self::TimeOffsets { .. })
                && matches!(error, FileSystemError::NotFound)
            {
                DeviceError::Errno(errno::ESRCH)
            } else {
                filesystem_error(error)
            }
        })?;
        let Some(bytes) = usize::try_from(offset)
            .ok()
            .and_then(|offset| contents.get(offset..))
        else {
            return Ok(());
        };
        let length = bytes.len().min(output.remaining());
        output
            .write(&bytes[..length])
            .map_err(|_| DeviceError::Errno(errno::EFAULT))
    }
    fn write_at(
        &self,
        input: &mut dyn UserInput,
        offset: u64,
        writer: &NamespaceAuthority,
    ) -> Result<(), DeviceError> {
        if let Self::TimeOffsets { inode, tid, opener } = self {
            return super::time_offsets::write(inode.source.as_ref(), *tid, opener, input, offset);
        }
        if let Self::PidLimit { target } = self {
            if !target.may_set_limit(writer) {
                return Err(DeviceError::Errno(errno::EPERM));
            }
            return super::sysctl_limit::write_value(input, offset, 0, |limit| {
                target.set_limit(limit, writer).map_err(namespace_error)
            });
        }
        if let Self::IpcLimit { target, resource } = self {
            if !target.may_set_limits(writer) {
                return Err(DeviceError::Errno(errno::EPERM));
            }
            return match *resource {
                crate::ipc::IpcLimit::Message(resource) => {
                    super::sysctl_limit::write_value(input, offset, 0, |limit| {
                        target
                            .set_message_limit(resource, limit, writer)
                            .map_err(ipc_error)
                    })
                }
                crate::ipc::IpcLimit::Semaphores => {
                    super::sysctl_limit::write_signed_vector(input, offset, |values| {
                        target
                            .set_semaphore_limits(values, writer)
                            .map_err(ipc_error)
                    })
                }
                crate::ipc::IpcLimit::SharedMemory(resource) => {
                    let publish = |limit| {
                        target
                            .set_shared_memory_limit(resource, limit, writer)
                            .map_err(ipc_error)
                    };
                    if matches!(
                        resource,
                        crate::ipc::SharedMemoryLimit::Segments
                            | crate::ipc::SharedMemoryLimit::ForcedRemove
                    ) {
                        super::sysctl_limit::write_value(input, offset, 0, |limit| {
                            publish(u64::from(limit))
                        })
                    } else {
                        super::sysctl_limit::write_unsigned_long(input, offset, publish)
                    }
                }
            };
        }
        if let Self::InotifyLimit(resource) = self {
            super::sysctl_permissions::require(writer.effective_uid, 2)
                .map_err(|_| DeviceError::Errno(errno::EPERM))?;
            return super::sysctl_limit::write_value(input, offset, 0, |limit| {
                resource.set(limit, writer).map_err(|error| match error {
                    NamespaceError::Permission => DeviceError::Errno(errno::EPERM),
                    _ => DeviceError::Errno(errno::EINVAL),
                })
            });
        }
        if let Self::CoreBufferLimit { target, resource } = self {
            if !may_set_core_buffers(target, writer) {
                return Err(DeviceError::Errno(errno::EPERM));
            }
            return super::sysctl_limit::write_value(input, offset, resource.minimum(), |value| {
                resource.set(value).map_err(namespace_error)
            });
        }
        if let Self::OptmemLimit(target) = self {
            if !target.may_set_limits(writer) {
                return Err(DeviceError::Errno(errno::EPERM));
            }
            return super::sysctl_limit::write_value(input, offset, 0, |value| {
                target
                    .set_optmem_max(value, writer)
                    .map_err(namespace_error)
            });
        }
        if let Self::MountLimit = self {
            // proc_sys_call_handler每次IO复查当前EUID，失败是EPERM；open准入仍是EACCES。
            super::sysctl_permissions::require(writer.effective_uid, 2)
                .map_err(|_| DeviceError::Errno(errno::EPERM))?;
            return super::sysctl_limit::write_value(input, offset, 1, |limit| {
                crate::fs::set_mount_limit(limit).map_err(|_| DeviceError::Errno(errno::EINVAL))
            });
        }
        if let Self::Limit { target, resource } = self {
            return super::sysctl_limit::write(target, *resource, input, offset, writer);
        }
        let Self::Map {
            target,
            kind,
            opener,
        } = self
        else {
            return Err(DeviceError::Errno(errno::EINVAL));
        };
        let count = input.remaining();
        let maximum = if *kind == UserNamespaceMap::Setgroups {
            8
        } else {
            4096
        };
        if offset != 0 || count >= maximum {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(count)
            .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
        bytes.resize(count, 0);
        input
            .copy(&mut bytes)
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        if *kind == UserNamespaceMap::Setgroups {
            let (allow, tail) = if let Some(tail) = bytes.strip_prefix(b"allow") {
                (true, tail)
            } else if let Some(tail) = bytes.strip_prefix(b"deny") {
                (false, tail)
            } else {
                return Err(DeviceError::Errno(errno::EINVAL));
            };
            if !tail.iter().all(u8::is_ascii_whitespace) {
                return Err(DeviceError::Errno(errno::EINVAL));
            }
            target
                .control_setgroups(allow, opener)
                .map_err(namespace_error)?;
        } else {
            target
                .publish_map(*kind == UserNamespaceMap::Uid, &bytes, opener, writer)
                .map_err(namespace_error)?;
        }
        input.consume(count);
        Ok(())
    }
    fn seek(&self, current: u64, offset: i64, whence: u32) -> Result<u64, DeviceError> {
        let base = match whence {
            0 => 0,
            1 => current,
            _ => return Err(DeviceError::Errno(errno::EINVAL)),
        };
        let next = i128::from(base) + i128::from(offset);
        if !(0..=i128::from(i64::MAX)).contains(&next) {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        Ok(next as u64)
    }
}
