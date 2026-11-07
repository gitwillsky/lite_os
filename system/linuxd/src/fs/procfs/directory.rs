//! proc 目录枚举；目录游标、实际 task namespace 与入口身份在同一次读取中解析。
use super::*;

impl ProcInode {
    pub(super) fn read_entries(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        let parent_node = match self.node {
            ProcNode::ProcessTaskDir(pid) => ProcNode::ProcessDir(pid),
            ProcNode::SysDir => ProcNode::Root,
            ProcNode::UserSysctlDir
            | ProcNode::FsSysctlDir
            | ProcNode::KernelSysctlDir
            | ProcNode::NetSysctlDir => ProcNode::SysDir,
            ProcNode::CoreNetSysctlDir => ProcNode::NetSysctlDir,
            ProcNode::ThreadDir(tgid, _, directory) => directory.parent(tgid),
            ProcNode::TaskFdDir(tgid, tid, directory)
            | ProcNode::TaskFdInfoDir(tgid, tid, directory)
            | ProcNode::NamespaceDir(tgid, tid, directory)
            | ProcNode::TaskNetDir(tgid, tid, directory) => directory.parent(tgid, tid),
            _ => ProcNode::Root,
        };
        // 原生 proc_net_readdir 在同一次目录读取固定 task 的实际 NET；nsproxy 消失返回 EINVAL。
        let network = if let ProcNode::TaskNetDir(_, tid, _) = self.node {
            Some(
                self.source
                    .thread_net_namespace(tid)
                    .ok_or(FileSystemError::InvalidOperation)?,
            )
        } else {
            None
        };
        let mut stream = IndexedDirectory::new(cursor, visitor);
        let mut index = 0usize;
        macro_rules! emit {
            ($node:expr, $kind:expr, $name:expr) => {{
                let entry_index = index;
                index += 1;
                if entry_index >= stream.start_index() {
                    let name: &[u8] = $name;
                    let inode =
                        self.child_inode($node, name != b"." && name != b"..", network.clone())?;
                    if !stream.emit(
                        entry_index,
                        DirectoryEntry {
                            inode: inode.inode_number(),
                            kind: $kind,
                            name,
                        },
                    )? {
                        return Ok(stream.finish());
                    }
                }
            }};
        }
        emit!(self.node, InodeType::Directory, b".");
        emit!(parent_node, InodeType::Directory, b"..");
        match self.node {
            ProcNode::Root => {
                for (node, kind, name) in [
                    (ProcNode::Stat, InodeType::File, &b"stat"[..]),
                    (ProcNode::MemInfo, InodeType::File, &b"meminfo"[..]),
                    (ProcNode::BuddyInfo, InodeType::File, &b"buddyinfo"[..]),
                    (ProcNode::VmStat, InodeType::File, &b"vmstat"[..]),
                    (ProcNode::LoadAvg, InodeType::File, &b"loadavg"[..]),
                    (ProcNode::Uptime, InodeType::File, &b"uptime"[..]),
                    (ProcNode::Mounts, InodeType::SymLink, &b"mounts"[..]),
                    (ProcNode::Partitions, InodeType::File, &b"partitions"[..]),
                    (ProcNode::Cmdline, InodeType::File, &b"cmdline"[..]),
                    (ProcNode::LastPanic, InodeType::File, &b"last_panic"[..]),
                    (ProcNode::NetLink, InodeType::SymLink, &b"net"[..]),
                    (ProcNode::SysDir, InodeType::Directory, &b"sys"[..]),
                    (ProcNode::SysvipcDir, InodeType::Directory, &b"sysvipc"[..]),
                    (ProcNode::SelfLink, InodeType::SymLink, &b"self"[..]),
                ] {
                    emit!(node, kind, name);
                }
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for process in self
                    .source
                    .process_identities(&self.pid_namespace)?
                    .into_iter()
                    .skip(start)
                {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ProcessDir(process.key()),
                        InodeType::Directory,
                        decimal_name(process.number_in(&self.pid_namespace) as usize, &mut name)
                    );
                }
            }
            ProcNode::ProcessDir(pid) => {
                emit!(
                    ProcNode::TimeOffsets(pid),
                    InodeType::File,
                    b"timens_offsets"
                );
                for (node, kind, name) in [
                    (
                        ProcNode::NamespaceDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"ns"[..],
                    ),
                    (
                        ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Uid),
                        InodeType::File,
                        &b"uid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Gid),
                        InodeType::File,
                        &b"gid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Setgroups),
                        InodeType::File,
                        &b"setgroups"[..],
                    ),
                    (
                        ProcNode::TaskNetDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"net"[..],
                    ),
                    (ProcNode::ProcessStat(pid), InodeType::File, &b"stat"[..]),
                    (
                        ProcNode::ProcessStatus(pid),
                        InodeType::File,
                        &b"status"[..],
                    ),
                    (
                        ProcNode::ProcessCmdline(pid),
                        InodeType::File,
                        &b"cmdline"[..],
                    ),
                    (ProcNode::ProcessComm(pid), InodeType::File, &b"comm"[..]),
                    (ProcNode::ProcessStatm(pid), InodeType::File, &b"statm"[..]),
                    (
                        ProcNode::ProcessMaps(pid, ProcMapFormat::Maps),
                        InodeType::File,
                        &b"maps"[..],
                    ),
                    (
                        ProcNode::ProcessMaps(pid, ProcMapFormat::Smaps),
                        InodeType::File,
                        &b"smaps"[..],
                    ),
                    (
                        ProcNode::ProcessMaps(pid, ProcMapFormat::Rollup),
                        InodeType::File,
                        &b"smaps_rollup"[..],
                    ),
                    (ProcNode::ProcessIo(pid), InodeType::File, &b"io"[..]),
                    (
                        ProcNode::ProcessMounts(pid),
                        InodeType::File,
                        &b"mounts"[..],
                    ),
                    (
                        ProcNode::ProcessMountInfo(pid),
                        InodeType::File,
                        &b"mountinfo"[..],
                    ),
                    (
                        ProcNode::ProcessMountStats(pid),
                        InodeType::File,
                        &b"mountstats"[..],
                    ),
                    (ProcNode::TaskExe(pid), InodeType::SymLink, &b"exe"[..]),
                    (ProcNode::ProcessRoot(pid), InodeType::SymLink, &b"root"[..]),
                    (ProcNode::ProcessCwd(pid), InodeType::SymLink, &b"cwd"[..]),
                    (
                        ProcNode::ProcessTaskDir(pid),
                        InodeType::Directory,
                        &b"task"[..],
                    ),
                    (
                        ProcNode::TaskFdDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"fd"[..],
                    ),
                    (
                        ProcNode::TaskFdInfoDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"fdinfo"[..],
                    ),
                ] {
                    emit!(node, kind, name);
                }
            }
            ProcNode::NamespaceDir(_, tid, _) => {
                for kind in crate::namespace::NamespaceSelector::ALL {
                    let node = ProcNode::NamespaceLink(tid, kind);
                    emit!(node, InodeType::SymLink, kind.name());
                }
            }
            ProcNode::ProcessTaskDir(pid) => {
                let snapshot = self
                    .source
                    .snapshot(ProcSnapshotScope::Process(pid), &self.pid_namespace)?;
                let process = find_process(&snapshot, pid)?;
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for thread in process.threads.iter().skip(start) {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ThreadDir(pid, thread.tid.key(), ProcThreadDirectory::Group),
                        InodeType::Directory,
                        decimal_name(
                            thread.tid.number_in(&self.pid_namespace) as usize,
                            &mut name
                        )
                    );
                }
            }
            ProcNode::ThreadDir(tgid, tid, directory) => {
                emit!(
                    ProcNode::TimeOffsets(tid),
                    InodeType::File,
                    b"timens_offsets"
                );
                let snapshot = self
                    .source
                    .snapshot(ProcSnapshotScope::Process(tgid), &self.pid_namespace)?;
                let process = find_process(&snapshot, tgid)?;
                let _ = find_thread(process, tid)?;
                for (node, name) in [
                    (
                        ProcNode::NamespaceDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"ns"[..],
                    ),
                    (
                        ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Uid),
                        &b"uid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Gid),
                        &b"gid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Setgroups),
                        &b"setgroups"[..],
                    ),
                    (
                        ProcNode::TaskNetDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"net"[..],
                    ),
                    (ProcNode::ThreadStat(tgid, tid), &b"stat"[..]),
                    (ProcNode::ThreadStatus(tgid, tid), &b"status"[..]),
                    (ProcNode::ThreadCmdline(tgid, tid), &b"cmdline"[..]),
                    (ProcNode::ThreadComm(tgid, tid), &b"comm"[..]),
                    (ProcNode::ThreadStatm(tgid, tid), &b"statm"[..]),
                    (
                        ProcNode::ThreadMaps(tgid, tid, ProcMapFormat::Maps),
                        &b"maps"[..],
                    ),
                    (
                        ProcNode::ThreadMaps(tgid, tid, ProcMapFormat::Smaps),
                        &b"smaps"[..],
                    ),
                    (
                        ProcNode::ThreadMaps(tgid, tid, ProcMapFormat::Rollup),
                        &b"smaps_rollup"[..],
                    ),
                    (ProcNode::ThreadIo(tgid, tid), &b"io"[..]),
                    (ProcNode::ThreadMounts(tid), &b"mounts"[..]),
                    (ProcNode::ThreadMountInfo(tid), &b"mountinfo"[..]),
                    (ProcNode::ThreadMountStats(tid), &b"mountstats"[..]),
                    (ProcNode::ThreadRoot(tid), &b"root"[..]),
                    (ProcNode::ThreadCwd(tid), &b"cwd"[..]),
                    (ProcNode::TaskExe(tid), &b"exe"[..]),
                    (
                        ProcNode::TaskFdDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"fd"[..],
                    ),
                    (
                        ProcNode::TaskFdInfoDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"fdinfo"[..],
                    ),
                ] {
                    emit!(node, node.kind(), name);
                }
            }
            ProcNode::TaskFdDir(_, pid, _) | ProcNode::TaskFdInfoDir(_, pid, _) => {
                let descriptors = self
                    .source
                    .thread_file_descriptors(pid)?
                    .ok_or(FileSystemError::NotFound)?;
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for entry in descriptors.into_iter().skip(start) {
                    let mut name = [0u8; 20];
                    let node = if matches!(self.node, ProcNode::TaskFdInfoDir(..)) {
                        ProcNode::TaskFdInfo(pid, entry.fd)
                    } else {
                        ProcNode::TaskFd(pid, entry.fd)
                    };
                    emit!(node, node.kind(), decimal_name(entry.fd, &mut name));
                }
            }
            ProcNode::SysDir => {
                emit!(ProcNode::KernelSysctlDir, InodeType::Directory, b"kernel");
                emit!(ProcNode::FsSysctlDir, InodeType::Directory, b"fs");
                emit!(ProcNode::UserSysctlDir, InodeType::Directory, b"user");
                emit!(ProcNode::NetSysctlDir, InodeType::Directory, b"net");
            }
            ProcNode::NetSysctlDir => {
                emit!(ProcNode::CoreNetSysctlDir, InodeType::Directory, b"core");
            }
            ProcNode::CoreNetSysctlDir => {
                emit!(ProcNode::OptmemLimit, InodeType::File, b"optmem_max");
                for resource in crate::net::CoreBufferLimit::ALL {
                    emit!(
                        ProcNode::CoreBufferLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::KernelSysctlDir => {
                emit!(ProcNode::PidLimit, InodeType::File, b"pid_max");
                for resource in crate::ipc::IpcLimit::ALL {
                    emit!(
                        ProcNode::IpcLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::FsSysctlDir => {
                emit!(ProcNode::MountLimit, InodeType::File, b"mount-max");
                emit!(ProcNode::InotifySysctlDir, InodeType::Directory, b"inotify");
            }
            ProcNode::InotifySysctlDir => {
                for resource in crate::fs::InotifyLimit::ALL {
                    emit!(
                        ProcNode::InotifyLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::UserSysctlDir => {
                for resource in [
                    crate::namespace::UserResource::User,
                    crate::namespace::UserResource::Uts,
                    crate::namespace::UserResource::Mount,
                    crate::namespace::UserResource::Time,
                    crate::namespace::UserResource::Pid,
                    crate::namespace::UserResource::Ipc,
                    crate::namespace::UserResource::Net,
                    crate::namespace::UserResource::InotifyInstances,
                    crate::namespace::UserResource::InotifyWatches,
                ] {
                    emit!(
                        ProcNode::UserResourceLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::SysvipcDir => {
                for table in crate::ipc::IpcTable::ALL {
                    emit!(ProcNode::IpcTable(table), InodeType::File, table.name());
                }
            }
            ProcNode::TaskNetDir(_, _, _) => {
                emit!(ProcNode::NetDev, InodeType::File, b"dev");
                emit!(ProcNode::NetRoute, InodeType::File, b"route");
            }
            _ => return Err(FileSystemError::NotDirectory),
        }
        let _ = index;
        Ok(stream.finish())
    }
}
