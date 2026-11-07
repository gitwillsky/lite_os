use super::*;

pub(super) fn decimal_name(value: usize, output: &mut [u8; 20]) -> &[u8] {
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

pub(super) fn find_process(
    snapshot: &ProcSnapshot,
    pid: usize,
) -> Result<&ProcProcessSnapshot, FileSystemError> {
    snapshot
        .processes
        .iter()
        .find(|process| process.pid.key() == pid)
        .ok_or(FileSystemError::NotFound)
}

pub(super) fn find_thread(
    process: &ProcProcessSnapshot,
    tid: usize,
) -> Result<&ProcThreadSnapshot, FileSystemError> {
    process
        .threads
        .iter()
        .find(|thread| thread.tid.key() == tid)
        .ok_or(FileSystemError::NotFound)
}

pub(super) fn parse_pid(name: &[u8]) -> Option<usize> {
    if name.is_empty() || name.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    name.iter().try_fold(0usize, |pid, byte| {
        pid.checked_mul(10)?.checked_add((byte - b'0') as usize)
    })
}

impl ProcInode {
    pub(super) fn lookup_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let mut network = None;
        let node = match self.node {
            ProcNode::Root => match name {
                b"." | b".." => ProcNode::Root,
                b"stat" => ProcNode::Stat,
                b"meminfo" => ProcNode::MemInfo,
                b"buddyinfo" => ProcNode::BuddyInfo,
                b"vmstat" => ProcNode::VmStat,
                b"loadavg" => ProcNode::LoadAvg,
                b"uptime" => ProcNode::Uptime,
                b"mounts" => ProcNode::Mounts,
                b"partitions" => ProcNode::Partitions,
                b"cmdline" => ProcNode::Cmdline,
                b"last_panic" => ProcNode::LastPanic,
                b"net" => ProcNode::NetLink,
                b"self" => ProcNode::SelfLink,
                b"sys" => ProcNode::SysDir,
                b"sysvipc" => ProcNode::SysvipcDir,
                _ => {
                    let pid = parse_pid(name)
                        .and_then(|number| u32::try_from(number).ok())
                        .ok_or(FileSystemError::NotFound)?;
                    let identity = self
                        .source
                        .task_identity(&self.pid_namespace, pid)
                        .ok_or(FileSystemError::NotFound)?;
                    if identity.process.key() == identity.thread.key() {
                        ProcNode::ProcessDir(identity.process.key())
                    } else {
                        ProcNode::ThreadDir(
                            identity.process.key(),
                            identity.thread.key(),
                            ProcThreadDirectory::Root,
                        )
                    }
                }
            },
            ProcNode::ProcessDir(pid) => match name {
                b"." => ProcNode::ProcessDir(pid),
                b".." => ProcNode::Root,
                b"stat" => ProcNode::ProcessStat(pid),
                b"status" => ProcNode::ProcessStatus(pid),
                b"cmdline" => ProcNode::ProcessCmdline(pid),
                b"comm" => ProcNode::ProcessComm(pid),
                b"statm" => ProcNode::ProcessStatm(pid),
                b"maps" => ProcNode::ProcessMaps(pid, ProcMapFormat::Maps),
                b"smaps" => ProcNode::ProcessMaps(pid, ProcMapFormat::Smaps),
                b"smaps_rollup" => ProcNode::ProcessMaps(pid, ProcMapFormat::Rollup),
                b"io" => ProcNode::ProcessIo(pid),
                b"mounts" => ProcNode::ProcessMounts(pid),
                b"mountinfo" => ProcNode::ProcessMountInfo(pid),
                b"mountstats" => ProcNode::ProcessMountStats(pid),
                b"exe" => ProcNode::TaskExe(pid),
                b"root" => ProcNode::ProcessRoot(pid),
                b"cwd" => ProcNode::ProcessCwd(pid),
                b"ns" => ProcNode::NamespaceDir(pid, pid, ProcTaskDirectory::Process),
                b"net" => ProcNode::TaskNetDir(pid, pid, ProcTaskDirectory::Process),
                b"timens_offsets" => ProcNode::TimeOffsets(pid),
                b"uid_map" => ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Uid),
                b"gid_map" => ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Gid),
                b"setgroups" => {
                    ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Setgroups)
                }
                b"task" => ProcNode::ProcessTaskDir(pid),
                b"fd" => ProcNode::TaskFdDir(pid, pid, ProcTaskDirectory::Process),
                b"fdinfo" => ProcNode::TaskFdInfoDir(pid, pid, ProcTaskDirectory::Process),
                _ => return Err(FileSystemError::NotFound),
            },
            ProcNode::ProcessTaskDir(tgid) => match name {
                b"." => ProcNode::ProcessTaskDir(tgid),
                b".." => ProcNode::ProcessDir(tgid),
                _ => {
                    let tid = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    let snapshot = self
                        .source
                        .snapshot(ProcSnapshotScope::Process(tgid), &self.pid_namespace)?;
                    let process = find_process(&snapshot, tgid)?;
                    let thread = process
                        .threads
                        .iter()
                        .find(|thread| thread.tid.number_in(&self.pid_namespace) as usize == tid)
                        .ok_or(FileSystemError::NotFound)?;
                    ProcNode::ThreadDir(tgid, thread.tid.key(), ProcThreadDirectory::Group)
                }
            },
            ProcNode::ThreadDir(tgid, tid, directory) => match name {
                b"." => self.node,
                b".." => directory.parent(tgid),
                b"ns" => ProcNode::NamespaceDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                b"net" => ProcNode::TaskNetDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                b"timens_offsets" => ProcNode::TimeOffsets(tid),
                b"uid_map" => ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Uid),
                b"gid_map" => ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Gid),
                b"setgroups" => {
                    ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Setgroups)
                }
                b"stat" => ProcNode::ThreadStat(tgid, tid),
                b"status" => ProcNode::ThreadStatus(tgid, tid),
                b"cmdline" => ProcNode::ThreadCmdline(tgid, tid),
                b"comm" => ProcNode::ThreadComm(tgid, tid),
                b"statm" => ProcNode::ThreadStatm(tgid, tid),
                b"maps" => ProcNode::ThreadMaps(tgid, tid, ProcMapFormat::Maps),
                b"smaps" => ProcNode::ThreadMaps(tgid, tid, ProcMapFormat::Smaps),
                b"smaps_rollup" => ProcNode::ThreadMaps(tgid, tid, ProcMapFormat::Rollup),
                b"io" => ProcNode::ThreadIo(tgid, tid),
                b"mounts" => ProcNode::ThreadMounts(tid),
                b"mountinfo" => ProcNode::ThreadMountInfo(tid),
                b"mountstats" => ProcNode::ThreadMountStats(tid),
                b"root" => ProcNode::ThreadRoot(tid),
                b"cwd" => ProcNode::ThreadCwd(tid),
                b"exe" => ProcNode::TaskExe(tid),
                b"fd" => ProcNode::TaskFdDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                b"fdinfo" => {
                    ProcNode::TaskFdInfoDir(tgid, tid, ProcTaskDirectory::Thread(directory))
                }
                _ => return Err(FileSystemError::NotFound),
            },
            ProcNode::NamespaceDir(tgid, tid, directory) => match name {
                b"." => self.node,
                b".." => directory.parent(tgid, tid),
                _ => ProcNode::NamespaceLink(
                    tid,
                    crate::namespace::NamespaceSelector::ALL
                        .into_iter()
                        .find(|selector| selector.name() == name)
                        .ok_or(FileSystemError::NotFound)?,
                ),
            },
            ProcNode::TaskFdDir(tgid, pid, directory)
            | ProcNode::TaskFdInfoDir(tgid, pid, directory) => match name {
                b"." => self.node,
                b".." => directory.parent(tgid, pid),
                _ => {
                    let fd = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    if !self
                        .source
                        .thread_file_descriptors(pid)?
                        .is_some_and(|entries| entries.iter().any(|entry| entry.fd == fd))
                    {
                        return Err(FileSystemError::NotFound);
                    }
                    if matches!(self.node, ProcNode::TaskFdInfoDir(..)) {
                        ProcNode::TaskFdInfo(pid, fd)
                    } else {
                        ProcNode::TaskFd(pid, fd)
                    }
                }
            },
            ProcNode::SysvipcDir => match name {
                b"." => ProcNode::SysvipcDir,
                b".." => ProcNode::Root,
                _ => ProcNode::IpcTable(
                    crate::ipc::IpcTable::ALL
                        .into_iter()
                        .find(|table| table.name() == name)
                        .ok_or(FileSystemError::NotFound)?,
                ),
            },
            ProcNode::SysDir if name == b"kernel" => ProcNode::KernelSysctlDir,
            ProcNode::SysDir if name == b"user" => ProcNode::UserSysctlDir,
            ProcNode::SysDir if name == b"fs" => ProcNode::FsSysctlDir,
            ProcNode::SysDir if name == b"net" => ProcNode::NetSysctlDir,
            ProcNode::SysDir => return Err(FileSystemError::NotFound),
            ProcNode::NetSysctlDir if name == b"core" => ProcNode::CoreNetSysctlDir,
            ProcNode::NetSysctlDir => return Err(FileSystemError::NotFound),
            ProcNode::CoreNetSysctlDir if name == b"optmem_max" => ProcNode::OptmemLimit,
            ProcNode::CoreNetSysctlDir => ProcNode::CoreBufferLimit(
                crate::net::CoreBufferLimit::ALL
                    .into_iter()
                    .find(|resource| resource.name() == name)
                    .ok_or(FileSystemError::NotFound)?,
            ),
            ProcNode::KernelSysctlDir if name == b"pid_max" => ProcNode::PidLimit,
            ProcNode::KernelSysctlDir => ProcNode::IpcLimit(
                crate::ipc::IpcLimit::ALL
                    .into_iter()
                    .find(|resource| resource.name() == name)
                    .ok_or(FileSystemError::NotFound)?,
            ),
            ProcNode::FsSysctlDir if name == b"mount-max" => ProcNode::MountLimit,
            ProcNode::FsSysctlDir if name == b"inotify" => ProcNode::InotifySysctlDir,
            ProcNode::FsSysctlDir => return Err(FileSystemError::NotFound),
            ProcNode::InotifySysctlDir => match name {
                b"." => ProcNode::InotifySysctlDir,
                b".." => ProcNode::FsSysctlDir,
                _ => ProcNode::InotifyLimit(
                    crate::fs::InotifyLimit::ALL
                        .into_iter()
                        .find(|resource| resource.name() == name)
                        .ok_or(FileSystemError::NotFound)?,
                ),
            },
            ProcNode::UserSysctlDir => ProcNode::UserResourceLimit(
                [
                    crate::namespace::UserResource::User,
                    crate::namespace::UserResource::Uts,
                    crate::namespace::UserResource::Mount,
                    crate::namespace::UserResource::Time,
                    crate::namespace::UserResource::Pid,
                    crate::namespace::UserResource::Ipc,
                    crate::namespace::UserResource::Net,
                    crate::namespace::UserResource::InotifyInstances,
                    crate::namespace::UserResource::InotifyWatches,
                ]
                .into_iter()
                .find(|resource| resource.name() == name)
                .ok_or(FileSystemError::NotFound)?,
            ),
            ProcNode::TaskNetDir(tgid, tid, directory) => {
                network = Some(
                    self.source
                        .thread_net_namespace(tid)
                        .ok_or(FileSystemError::NotFound)?,
                );
                match name {
                    b"." => self.node,
                    b".." => directory.parent(tgid, tid),
                    b"dev" => ProcNode::NetDev,
                    b"route" => ProcNode::NetRoute,
                    _ => return Err(FileSystemError::NotFound),
                }
            }
            _ => return Err(FileSystemError::NotDirectory),
        };
        Ok(self.child_inode(node, name != b"." && name != b"..", network)?)
    }
}
