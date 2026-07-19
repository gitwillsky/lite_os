use alloc::{sync::Arc, vec::Vec};
use core::fmt::{self, Write};

mod lookup;
mod node;
mod process;
mod snapshot;
mod system;
use lookup::{decimal_name, find_process, find_thread, parse_pid};
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
    format_buddyinfo, format_cpu_stat, format_loadavg, format_meminfo, format_network_devices,
    format_network_routes, format_uptime, format_vmstat,
};

use super::{
    DirectoryEntry, DirectoryRead, DirectoryVisitor, FileSystem, FileSystemError,
    FileSystemStatistics, IndexedDirectory, Inode, InodeMetadata, InodeType, vfs,
};

const PROC_FILESYSTEM_ID: usize = 3;

pub(super) struct ProcText(Vec<u8>);

impl ProcText {
    pub(super) const fn new() -> Self {
        Self(Vec::new())
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.0
    }
}

impl Write for ProcText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

pub(super) fn proc_text(arguments: fmt::Arguments<'_>) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    output
        .write_fmt(arguments)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(output.finish())
}

/// @description procfs 读取 kernel 状态的窄接口；状态仍由 task、memory 与 processor 唯一拥有。
pub(crate) trait ProcSource: Send + Sync {
    /// @description 在一次读取边界取得自洽的只读快照。
    fn snapshot(&self) -> Result<ProcSnapshot, FileSystemError>;

    /// @description 返回正在解析 `/proc/self` 的 calling process TGID。
    /// @return user process context 返回 TGID；无 current task 返回 None。
    fn current_pid(&self) -> Option<usize>;

    /// @description 按 TGID 从目标 MemorySet argument range 读取实时 argv bytes。
    /// @param pid live process TGID。
    /// @return 存在且 argument range 可读时返回 NUL 分隔 bytes；不存在返回 None。
    /// @errors kernel snapshot buffer OOM 返回明确文件系统错误。
    fn process_arguments(&self, pid: usize) -> Result<Option<Vec<u8>>, FileSystemError>;

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
    fn new(source: Arc<dyn ProcSource>, node: ProcNode) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self { source, node }).map_err(|_| FileSystemError::OutOfMemory)
    }

    fn file_contents(&self) -> Result<Vec<u8>, FileSystemError> {
        if matches!(self.node, ProcNode::Mounts) {
            return vfs().mount_table();
        }
        if let ProcNode::ProcessCmdline(pid) = self.node {
            return self
                .source
                .process_arguments(pid)?
                .ok_or(FileSystemError::NotFound);
        }
        if let ProcNode::ThreadCmdline(tgid, tid) = self.node {
            let snapshot = self.source.snapshot()?;
            let process = find_process(&snapshot, tgid)?;
            let _ = find_thread(process, tid)?;
            return self
                .source
                .process_arguments(tgid)?
                .ok_or(FileSystemError::NotFound);
        }
        let snapshot = self.source.snapshot()?;
        match self.node {
            ProcNode::Stat => format_cpu_stat(&snapshot),
            ProcNode::MemInfo => format_meminfo(&snapshot),
            ProcNode::BuddyInfo => format_buddyinfo(&snapshot),
            ProcNode::VmStat => format_vmstat(&snapshot),
            ProcNode::LoadAvg => format_loadavg(&snapshot),
            ProcNode::Uptime => format_uptime(&snapshot),
            ProcNode::NetDev => format_network_devices(snapshot.network),
            ProcNode::NetRoute => format_network_routes(snapshot.network),
            ProcNode::Mounts => unreachable!("mount table handled before task snapshot"),
            ProcNode::ProcessStat(pid) => format_process_stat(find_process(&snapshot, pid)?),
            ProcNode::ProcessStatus(pid) => format_process_status(find_process(&snapshot, pid)?),
            ProcNode::ProcessComm(pid) => format_process_comm(find_process(&snapshot, pid)?),
            ProcNode::ProcessStatm(pid) => format_process_statm(find_process(&snapshot, pid)?),
            ProcNode::ProcessIo(pid) => format_io(&find_process(&snapshot, pid)?.io),
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
            | ProcNode::ProcessFd(_, _) => Err(FileSystemError::IsDirectory),
            ProcNode::ProcessCmdline(_) | ProcNode::ThreadCmdline(_, _) => {
                unreachable!("cmdline handled as binary data")
            }
        }
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
            ProcNode::SelfLink => {
                let pid = self.source.current_pid().ok_or(FileSystemError::NotFound)?;
                let mut stack = [0u8; 20];
                let name = decimal_name(pid, &mut stack);
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(name.len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                bytes.extend_from_slice(name);
                Ok(bytes)
            }
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

    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        let parent_inode = match self.node {
            ProcNode::ProcessFdDir(pid) | ProcNode::ProcessTaskDir(pid) => {
                ProcNode::ProcessDir(pid).inode()
            }
            ProcNode::ThreadDir(tgid, _) => ProcNode::ProcessTaskDir(tgid).inode(),
            _ => 1,
        };
        let mut stream = IndexedDirectory::new(cursor, visitor);
        let mut index = 0usize;
        macro_rules! emit {
            ($inode:expr, $kind:expr, $name:expr) => {{
                let entry_index = index;
                index += 1;
                if !stream.emit(
                    entry_index,
                    DirectoryEntry {
                        inode: $inode,
                        kind: $kind,
                        name: $name,
                    },
                )? {
                    return Ok(stream.finish());
                }
            }};
        }
        emit!(self.node.inode(), InodeType::Directory, b".");
        emit!(parent_inode, InodeType::Directory, b"..");
        match self.node {
            ProcNode::Root => {
                for (inode, kind, name) in [
                    (2, InodeType::File, &b"stat"[..]),
                    (3, InodeType::File, &b"meminfo"[..]),
                    (11, InodeType::File, &b"buddyinfo"[..]),
                    (12, InodeType::File, &b"vmstat"[..]),
                    (4, InodeType::File, &b"loadavg"[..]),
                    (5, InodeType::File, &b"uptime"[..]),
                    (6, InodeType::File, &b"mounts"[..]),
                    (7, InodeType::Directory, &b"net"[..]),
                    (10, InodeType::SymLink, &b"self"[..]),
                ] {
                    emit!(inode, kind, name);
                }
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for process in self.source.snapshot()?.processes.into_iter().skip(start) {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ProcessDir(process.pid).inode(),
                        InodeType::Directory,
                        decimal_name(process.pid, &mut name)
                    );
                }
            }
            ProcNode::ProcessDir(pid) => {
                for (node, kind, name) in [
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
                    (ProcNode::ProcessIo(pid), InodeType::File, &b"io"[..]),
                    (
                        ProcNode::ProcessTaskDir(pid),
                        InodeType::Directory,
                        &b"task"[..],
                    ),
                    (
                        ProcNode::ProcessFdDir(pid),
                        InodeType::Directory,
                        &b"fd"[..],
                    ),
                ] {
                    emit!(node.inode(), kind, name);
                }
            }
            ProcNode::ProcessTaskDir(pid) => {
                let snapshot = self.source.snapshot()?;
                let process = find_process(&snapshot, pid)?;
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for thread in process.threads.iter().skip(start) {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ThreadDir(pid, thread.tid).inode(),
                        InodeType::Directory,
                        decimal_name(thread.tid, &mut name)
                    );
                }
            }
            ProcNode::ThreadDir(tgid, tid) => {
                let snapshot = self.source.snapshot()?;
                let process = find_process(&snapshot, tgid)?;
                let _ = find_thread(process, tid)?;
                for (node, name) in [
                    (ProcNode::ThreadStat(tgid, tid), &b"stat"[..]),
                    (ProcNode::ThreadStatus(tgid, tid), &b"status"[..]),
                    (ProcNode::ThreadCmdline(tgid, tid), &b"cmdline"[..]),
                    (ProcNode::ThreadComm(tgid, tid), &b"comm"[..]),
                    (ProcNode::ThreadStatm(tgid, tid), &b"statm"[..]),
                    (ProcNode::ThreadIo(tgid, tid), &b"io"[..]),
                ] {
                    emit!(node.inode(), InodeType::File, name);
                }
            }
            ProcNode::ProcessFdDir(pid) => {
                let descriptors = self
                    .source
                    .process_file_descriptors(pid)?
                    .ok_or(FileSystemError::NotFound)?;
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for entry in descriptors.into_iter().skip(start) {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ProcessFd(pid, entry.fd).inode(),
                        InodeType::SymLink,
                        decimal_name(entry.fd, &mut name)
                    );
                }
            }
            ProcNode::NetDir => {
                emit!(8, InodeType::File, b"dev");
                emit!(9, InodeType::File, b"route");
            }
            _ => return Err(FileSystemError::NotDirectory),
        }
        let _ = index;
        Ok(stream.finish())
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
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
                b"net" => ProcNode::NetDir,
                b"self" => ProcNode::SelfLink,
                _ => {
                    let pid = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    if !self
                        .source
                        .snapshot()?
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
                    let snapshot = self.source.snapshot()?;
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
        Ok(Self::new(self.source.clone(), node)?)
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
    pub(crate) fn new(source: Arc<dyn ProcSource>) -> Result<Arc<Self>, FileSystemError> {
        let root = ProcInode::new(source, ProcNode::Root)?;
        Arc::try_new(Self { root }).map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl FileSystem for ProcFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(FileSystemStatistics {
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
        })
    }
}
