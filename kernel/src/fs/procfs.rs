use alloc::{string::ToString, sync::Arc, vec, vec::Vec};

mod lookup;
mod node;
mod process;
mod snapshot;
mod system;
use lookup::{directory_entry, find_process, find_thread, parse_pid};
use node::ProcNode;
use process::{
    format_io, format_process_comm, format_process_stat, format_process_statm,
    format_process_status, format_thread_stat, format_thread_status,
};
pub(crate) use snapshot::{
    ProcCpuSnapshot, ProcFileDescriptorSnapshot, ProcIoSnapshot, ProcNetworkSnapshot,
    ProcProcessSnapshot, ProcSnapshot, ProcThreadSnapshot,
};
use system::{
    format_cpu_stat, format_loadavg, format_meminfo, format_network_devices, format_network_routes,
    format_uptime,
};

use super::{
    DirectoryEntry, FileSystem, FileSystemError, FileSystemStatistics, Inode, InodeMetadata,
    InodeType, vfs,
};

const PROC_FILESYSTEM_ID: usize = 3;

/// @description procfs 读取 kernel 状态的窄接口；状态仍由 task、memory 与 processor 唯一拥有。
pub(crate) trait ProcSource: Send + Sync {
    /// @description 在一次读取边界取得自洽的只读快照。
    fn snapshot(&self) -> ProcSnapshot;

    /// @description 返回正在解析 `/proc/self` 的 calling process TGID。
    /// @return user process context 返回 TGID；无 current task 返回 None。
    fn current_pid(&self) -> Option<usize>;

    /// @description 按 TGID 从目标 MemorySet argument range 读取实时 argv bytes。
    /// @param pid live process TGID。
    /// @return 存在且 argument range 可读时返回 NUL 分隔 bytes；否则返回 None。
    fn process_arguments(&self, pid: usize) -> Option<Vec<u8>>;

    /// @description 按 TGID 投影 live fd/OFD identity，不复制 backend 状态。
    /// @param pid live process TGID。
    /// @return process 存在且快照成功时返回按 fd 排序的 targets；否则返回 None。
    fn process_file_descriptors(
        &self,
        pid: usize,
    ) -> Result<Option<Vec<ProcFileDescriptorSnapshot>>, FileSystemError>;
}

struct ProcInode {
    source: Arc<dyn ProcSource>,
    node: ProcNode,
}

impl ProcInode {
    fn new(source: Arc<dyn ProcSource>, node: ProcNode) -> Arc<Self> {
        Arc::new(Self { source, node })
    }

    fn file_contents(&self) -> Result<Vec<u8>, FileSystemError> {
        if matches!(self.node, ProcNode::Mounts) {
            return vfs().mount_table();
        }
        if let ProcNode::ProcessCmdline(pid) = self.node {
            return self
                .source
                .process_arguments(pid)
                .ok_or(FileSystemError::NotFound);
        }
        if let ProcNode::ThreadCmdline(tgid, tid) = self.node {
            let snapshot = self.source.snapshot();
            let process = find_process(&snapshot, tgid)?;
            let _ = find_thread(process, tid)?;
            return self
                .source
                .process_arguments(tgid)
                .ok_or(FileSystemError::NotFound);
        }
        let snapshot = self.source.snapshot();
        let text = match self.node {
            ProcNode::Stat => format_cpu_stat(&snapshot),
            ProcNode::MemInfo => format_meminfo(&snapshot),
            ProcNode::LoadAvg => format_loadavg(&snapshot),
            ProcNode::Uptime => format_uptime(&snapshot),
            ProcNode::NetDev => format_network_devices(snapshot.network),
            ProcNode::NetRoute => format_network_routes(snapshot.network),
            ProcNode::Mounts => unreachable!("mount table handled before task snapshot"),
            ProcNode::ProcessStat(pid) => snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(format_process_stat)
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::ProcessStatus(pid) => snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(format_process_status)
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::ProcessComm(pid) => snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(format_process_comm)
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::ProcessStatm(pid) => snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(format_process_statm)
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::ProcessIo(pid) => snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(|process| format_io(&process.io))
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::ThreadStat(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_thread_stat(process, find_thread(process, tid)?)
            }
            ProcNode::ThreadStatus(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_thread_status(process, find_thread(process, tid)?)
            }
            ProcNode::ThreadComm(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                let _ = find_thread(process, tid)?;
                format_process_comm(process)
            }
            ProcNode::ThreadStatm(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                let _ = find_thread(process, tid)?;
                format_process_statm(process)
            }
            ProcNode::ThreadIo(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_io(&find_thread(process, tid)?.io)
            }
            ProcNode::Root
            | ProcNode::NetDir
            | ProcNode::SelfLink
            | ProcNode::ProcessDir(_)
            | ProcNode::ProcessTaskDir(_)
            | ProcNode::ProcessFdDir(_)
            | ProcNode::ThreadDir(_, _)
            | ProcNode::ProcessFd(_, _) => {
                return Err(FileSystemError::IsDirectory);
            }
            ProcNode::ProcessCmdline(_) | ProcNode::ThreadCmdline(_, _) => {
                unreachable!("cmdline handled as binary data")
            }
        };
        Ok(text.into_bytes())
    }
}

impl Inode for ProcInode {
    fn filesystem_id(&self) -> usize {
        PROC_FILESYSTEM_ID
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let kind = self.node.kind();
        let size = match kind {
            InodeType::File => self.file_contents()?.len() as u64,
            InodeType::SymLink => self.read_link()?.len() as u64,
            _ => 0,
        };
        Ok(InodeMetadata {
            filesystem: PROC_FILESYSTEM_ID as u64,
            inode: self.node.inode(),
            kind,
            mode: match kind {
                InodeType::Directory => 0o040555,
                InodeType::SymLink => 0o120777,
                _ => 0o100444,
            },
            links: if kind == InodeType::Directory { 2 } else { 1 },
            uid: 0,
            gid: 0,
            size,
            blocks: 0,
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        self.node.kind()
    }
    fn size(&self) -> u64 {
        self.file_contents()
            .map_or(0, |contents| contents.len() as u64)
    }
    fn is_executable(&self) -> bool {
        false
    }

    fn is_volatile(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let contents = self.file_contents()?;
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        if offset >= contents.len() {
            return Ok(0);
        }
        let length = buf.len().min(contents.len() - offset);
        buf[..length].copy_from_slice(&contents[offset..offset + length]);
        Ok(length)
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        match self.node {
            ProcNode::SelfLink => self
                .source
                .current_pid()
                .map(|pid| pid.to_string().into_bytes())
                .ok_or(FileSystemError::NotFound),
            ProcNode::ProcessFd(pid, fd) => self
                .source
                .process_file_descriptors(pid)?
                .and_then(|entries| entries.into_iter().find(|entry| entry.fd == fd))
                .map(|entry| entry.target)
                .ok_or(FileSystemError::NotFound),
            _ => Err(FileSystemError::InvalidOperation),
        }
    }

    fn follow_link(&self) -> Option<Arc<super::OpenedFile>> {
        let ProcNode::ProcessFd(pid, fd) = self.node else {
            return None;
        };
        self.source
            .process_file_descriptors(pid)
            .ok()??
            .into_iter()
            .find(|entry| entry.fd == fd)?
            .opened
    }

    fn write_storage(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn append_storage(&self, _buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn truncate_storage(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        let parent_inode = match self.node {
            ProcNode::ProcessFdDir(pid) | ProcNode::ProcessTaskDir(pid) => {
                ProcNode::ProcessDir(pid).inode()
            }
            ProcNode::ThreadDir(tgid, _) => ProcNode::ProcessTaskDir(tgid).inode(),
            _ => 1,
        };
        let mut entries = vec![
            directory_entry(self.node.inode(), InodeType::Directory, b"."),
            directory_entry(parent_inode, InodeType::Directory, b".."),
        ];
        match self.node {
            ProcNode::Root => {
                entries.extend([
                    directory_entry(2, InodeType::File, b"stat"),
                    directory_entry(3, InodeType::File, b"meminfo"),
                    directory_entry(4, InodeType::File, b"loadavg"),
                    directory_entry(5, InodeType::File, b"uptime"),
                    directory_entry(6, InodeType::File, b"mounts"),
                    directory_entry(7, InodeType::Directory, b"net"),
                    directory_entry(10, InodeType::SymLink, b"self"),
                ]);
                entries.extend(self.source.snapshot().processes.into_iter().map(|process| {
                    directory_entry(
                        ProcNode::ProcessDir(process.pid).inode(),
                        InodeType::Directory,
                        process.pid.to_string().as_bytes(),
                    )
                }));
            }
            ProcNode::ProcessDir(pid) => entries.extend([
                directory_entry(ProcNode::ProcessStat(pid).inode(), InodeType::File, b"stat"),
                directory_entry(
                    ProcNode::ProcessStatus(pid).inode(),
                    InodeType::File,
                    b"status",
                ),
                directory_entry(
                    ProcNode::ProcessCmdline(pid).inode(),
                    InodeType::File,
                    b"cmdline",
                ),
                directory_entry(ProcNode::ProcessComm(pid).inode(), InodeType::File, b"comm"),
                directory_entry(
                    ProcNode::ProcessStatm(pid).inode(),
                    InodeType::File,
                    b"statm",
                ),
                directory_entry(ProcNode::ProcessIo(pid).inode(), InodeType::File, b"io"),
                directory_entry(
                    ProcNode::ProcessTaskDir(pid).inode(),
                    InodeType::Directory,
                    b"task",
                ),
                directory_entry(
                    ProcNode::ProcessFdDir(pid).inode(),
                    InodeType::Directory,
                    b"fd",
                ),
            ]),
            ProcNode::ProcessTaskDir(pid) => {
                let snapshot = self.source.snapshot();
                let process = find_process(&snapshot, pid)?;
                entries.extend(process.threads.iter().map(|thread| {
                    directory_entry(
                        ProcNode::ThreadDir(pid, thread.tid).inode(),
                        InodeType::Directory,
                        thread.tid.to_string().as_bytes(),
                    )
                }));
            }
            ProcNode::ThreadDir(tgid, tid) => {
                let snapshot = self.source.snapshot();
                let process = find_process(&snapshot, tgid)?;
                let _ = find_thread(process, tid)?;
                entries.extend([
                    directory_entry(
                        ProcNode::ThreadStat(tgid, tid).inode(),
                        InodeType::File,
                        b"stat",
                    ),
                    directory_entry(
                        ProcNode::ThreadStatus(tgid, tid).inode(),
                        InodeType::File,
                        b"status",
                    ),
                    directory_entry(
                        ProcNode::ThreadCmdline(tgid, tid).inode(),
                        InodeType::File,
                        b"cmdline",
                    ),
                    directory_entry(
                        ProcNode::ThreadComm(tgid, tid).inode(),
                        InodeType::File,
                        b"comm",
                    ),
                    directory_entry(
                        ProcNode::ThreadStatm(tgid, tid).inode(),
                        InodeType::File,
                        b"statm",
                    ),
                    directory_entry(
                        ProcNode::ThreadIo(tgid, tid).inode(),
                        InodeType::File,
                        b"io",
                    ),
                ]);
            }
            ProcNode::ProcessFdDir(pid) => {
                entries.extend(
                    self.source
                        .process_file_descriptors(pid)?
                        .ok_or(FileSystemError::NotFound)?
                        .into_iter()
                        .map(|entry| {
                            directory_entry(
                                ProcNode::ProcessFd(pid, entry.fd).inode(),
                                InodeType::SymLink,
                                entry.fd.to_string().as_bytes(),
                            )
                        }),
                );
            }
            ProcNode::NetDir => entries.extend([
                directory_entry(8, InodeType::File, b"dev"),
                directory_entry(9, InodeType::File, b"route"),
            ]),
            _ => return Err(FileSystemError::NotDirectory),
        }
        Ok(entries)
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let node = match self.node {
            ProcNode::Root => match name {
                b"." | b".." => ProcNode::Root,
                b"stat" => ProcNode::Stat,
                b"meminfo" => ProcNode::MemInfo,
                b"loadavg" => ProcNode::LoadAvg,
                b"uptime" => ProcNode::Uptime,
                b"mounts" => ProcNode::Mounts,
                b"net" => ProcNode::NetDir,
                b"self" => ProcNode::SelfLink,
                _ => {
                    let pid = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    if !self
                        .source
                        .snapshot()
                        .processes
                        .iter()
                        .any(|process| process.pid == pid)
                    {
                        return Err(FileSystemError::NotFound);
                    }
                    ProcNode::ProcessDir(pid)
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
                b"io" => ProcNode::ProcessIo(pid),
                b"task" => ProcNode::ProcessTaskDir(pid),
                b"fd" => ProcNode::ProcessFdDir(pid),
                _ => return Err(FileSystemError::NotFound),
            },
            ProcNode::ProcessTaskDir(tgid) => match name {
                b"." => ProcNode::ProcessTaskDir(tgid),
                b".." => ProcNode::ProcessDir(tgid),
                _ => {
                    let tid = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    let snapshot = self.source.snapshot();
                    let process = find_process(&snapshot, tgid)?;
                    let _ = find_thread(process, tid)?;
                    ProcNode::ThreadDir(tgid, tid)
                }
            },
            ProcNode::ThreadDir(tgid, tid) => match name {
                b"." => ProcNode::ThreadDir(tgid, tid),
                b".." => ProcNode::ProcessTaskDir(tgid),
                b"stat" => ProcNode::ThreadStat(tgid, tid),
                b"status" => ProcNode::ThreadStatus(tgid, tid),
                b"cmdline" => ProcNode::ThreadCmdline(tgid, tid),
                b"comm" => ProcNode::ThreadComm(tgid, tid),
                b"statm" => ProcNode::ThreadStatm(tgid, tid),
                b"io" => ProcNode::ThreadIo(tgid, tid),
                _ => return Err(FileSystemError::NotFound),
            },
            ProcNode::ProcessFdDir(pid) => match name {
                b"." => ProcNode::ProcessFdDir(pid),
                b".." => ProcNode::ProcessDir(pid),
                _ => {
                    let fd = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    if !self
                        .source
                        .process_file_descriptors(pid)?
                        .is_some_and(|entries| entries.iter().any(|entry| entry.fd == fd))
                    {
                        return Err(FileSystemError::NotFound);
                    }
                    ProcNode::ProcessFd(pid, fd)
                }
            },
            ProcNode::NetDir => match name {
                b"." => ProcNode::NetDir,
                b".." => ProcNode::Root,
                b"dev" => ProcNode::NetDev,
                b"route" => ProcNode::NetRoute,
                _ => return Err(FileSystemError::NotFound),
            },
            _ => return Err(FileSystemError::NotDirectory),
        };
        Ok(Self::new(self.source.clone(), node))
    }

    fn create(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: super::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn unlink(&self, _name: &[u8], _remove_directory: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn rename(
        &self,
        _old_name: &[u8],
        _new_parent_inode: u64,
        _new_name: &[u8],
        _no_replace: bool,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
}

pub(crate) struct ProcFileSystem {
    root: Arc<ProcInode>,
}

impl ProcFileSystem {
    pub(crate) fn new(source: Arc<dyn ProcSource>) -> Arc<Self> {
        Arc::new(Self {
            root: ProcInode::new(source, ProcNode::Root),
        })
    }
}

impl FileSystem for ProcFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> FileSystemStatistics {
        FileSystemStatistics {
            type_name: "proc",
            magic: 0x9fa0,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [PROC_FILESYSTEM_ID as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 1,
        }
    }
}
