use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::fmt::Write;

use super::{DirectoryEntry, FileSystem, FileSystemError, Inode, InodeMetadata, InodeType};

const PROC_FILESYSTEM_ID: usize = 3;
const CLOCK_TICKS_PER_SECOND: u64 = 100;

#[derive(Clone)]
pub(crate) struct ProcProcessSnapshot {
    pub(crate) pid: usize,
    pub(crate) ppid: usize,
    pub(crate) process_group: usize,
    pub(crate) session: usize,
    pub(crate) comm: Vec<u8>,
    pub(crate) state: u8,
    pub(crate) nice: i32,
    pub(crate) priority: i32,
    pub(crate) threads: usize,
    pub(crate) runtime_us: u64,
    pub(crate) start_time_us: u64,
    pub(crate) virtual_pages: usize,
    pub(crate) resident_pages: usize,
    pub(crate) last_cpu: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct ProcCpuSnapshot {
    pub(crate) hart_id: usize,
    pub(crate) busy_us: u64,
}

pub(crate) struct ProcSnapshot {
    pub(crate) uptime_us: u64,
    pub(crate) total_pages: usize,
    pub(crate) free_pages: usize,
    pub(crate) runnable_tasks: usize,
    pub(crate) total_tasks: usize,
    pub(crate) processes_created: u64,
    pub(crate) last_pid: usize,
    pub(crate) load_milli: [u64; 3],
    pub(crate) cpus: Vec<ProcCpuSnapshot>,
    pub(crate) processes: Vec<ProcProcessSnapshot>,
}

/// @description procfs 读取 kernel 状态的窄接口；状态仍由 task、memory 与 processor 唯一拥有。
pub(crate) trait ProcSource: Send + Sync {
    /// @description 在一次读取边界取得自洽的只读快照。
    fn snapshot(&self) -> ProcSnapshot;
}

#[derive(Clone, Copy)]
enum ProcNode {
    Root,
    Stat,
    MemInfo,
    LoadAvg,
    Uptime,
    ProcessDir(usize),
    ProcessStat(usize),
}

impl ProcNode {
    fn inode(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Stat => 2,
            Self::MemInfo => 3,
            Self::LoadAvg => 4,
            Self::Uptime => 5,
            Self::ProcessDir(pid) => 0x1_0000 + (pid as u64) * 2,
            Self::ProcessStat(pid) => 0x1_0001 + (pid as u64) * 2,
        }
    }

    fn kind(self) -> InodeType {
        match self {
            Self::Root | Self::ProcessDir(_) => InodeType::Directory,
            _ => InodeType::File,
        }
    }
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
        let snapshot = self.source.snapshot();
        let text = match self.node {
            ProcNode::Stat => format_cpu_stat(&snapshot),
            ProcNode::MemInfo => format_meminfo(&snapshot),
            ProcNode::LoadAvg => format_loadavg(&snapshot),
            ProcNode::Uptime => format_uptime(&snapshot),
            ProcNode::ProcessStat(pid) => snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(format_process_stat)
                .ok_or(FileSystemError::NotFound)?,
            ProcNode::Root | ProcNode::ProcessDir(_) => return Err(FileSystemError::IsDirectory),
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
        let size = if kind == InodeType::File {
            self.file_contents()?.len() as u64
        } else {
            0
        };
        Ok(InodeMetadata {
            filesystem: PROC_FILESYSTEM_ID as u64,
            inode: self.node.inode(),
            kind,
            mode: if kind == InodeType::Directory {
                0o040555
            } else {
                0o100444
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

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let contents = self.file_contents()?;
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        if offset >= contents.len() {
            return Ok(0);
        }
        let length = buf.len().min(contents.len() - offset);
        buf[..length].copy_from_slice(&contents[offset..offset + length]);
        Ok(length)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn append(&self, _buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        let mut entries = vec![
            directory_entry(self.node.inode(), InodeType::Directory, b"."),
            directory_entry(1, InodeType::Directory, b".."),
        ];
        match self.node {
            ProcNode::Root => {
                entries.extend([
                    directory_entry(2, InodeType::File, b"stat"),
                    directory_entry(3, InodeType::File, b"meminfo"),
                    directory_entry(4, InodeType::File, b"loadavg"),
                    directory_entry(5, InodeType::File, b"uptime"),
                ]);
                entries.extend(self.source.snapshot().processes.into_iter().map(|process| {
                    directory_entry(
                        ProcNode::ProcessDir(process.pid).inode(),
                        InodeType::Directory,
                        process.pid.to_string().as_bytes(),
                    )
                }));
            }
            ProcNode::ProcessDir(pid) => entries.push(directory_entry(
                ProcNode::ProcessStat(pid).inode(),
                InodeType::File,
                b"stat",
            )),
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
        _mode: u32,
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
}

fn directory_entry(inode: u64, kind: InodeType, name: &[u8]) -> DirectoryEntry {
    DirectoryEntry {
        inode,
        kind,
        name: name.to_vec(),
    }
}

fn parse_pid(name: &[u8]) -> Option<usize> {
    if name.is_empty() || name.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    name.iter().try_fold(0usize, |pid, byte| {
        pid.checked_mul(10)?.checked_add((byte - b'0') as usize)
    })
}

fn ticks(microseconds: u64) -> u64 {
    microseconds / (1_000_000 / CLOCK_TICKS_PER_SECOND)
}

fn format_cpu_stat(snapshot: &ProcSnapshot) -> String {
    let mut output = String::new();
    let total_busy: u64 = snapshot
        .cpus
        .iter()
        .map(|cpu| cpu.busy_us.min(snapshot.uptime_us))
        .sum();
    let total_idle = snapshot
        .uptime_us
        .saturating_mul(snapshot.cpus.len() as u64)
        .saturating_sub(total_busy);
    let _ = writeln!(
        output,
        "cpu  {} 0 0 {} 0 0 0 0",
        ticks(total_busy),
        ticks(total_idle)
    );
    for cpu in &snapshot.cpus {
        let busy = cpu.busy_us.min(snapshot.uptime_us);
        let _ = writeln!(
            output,
            "cpu{} {} 0 0 {} 0 0 0 0",
            cpu.hart_id,
            ticks(busy),
            ticks(snapshot.uptime_us - busy)
        );
    }
    let _ = writeln!(
        output,
        "processes {}\nprocs_running {}\nprocs_blocked 0",
        snapshot.processes_created, snapshot.runnable_tasks
    );
    output
}

fn format_meminfo(snapshot: &ProcSnapshot) -> String {
    format!(
        "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\nBuffers:        0 kB\nCached:         0 kB\nSwapCached:     0 kB\nActive:         0 kB\nInactive:       0 kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\nDirty:          0 kB\nWriteback:      0 kB\nAnonPages:      0 kB\nMapped:         0 kB\nShmem:          0 kB\nSlab:           0 kB\n",
        snapshot.total_pages * 4,
        snapshot.free_pages * 4,
        snapshot.free_pages * 4
    )
}

fn format_loadavg(snapshot: &ProcSnapshot) -> String {
    format!(
        "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
        snapshot.load_milli[0] / 1000,
        snapshot.load_milli[0] / 10 % 100,
        snapshot.load_milli[1] / 1000,
        snapshot.load_milli[1] / 10 % 100,
        snapshot.load_milli[2] / 1000,
        snapshot.load_milli[2] / 10 % 100,
        snapshot.runnable_tasks,
        snapshot.total_tasks,
        snapshot.last_pid
    )
}

fn format_uptime(snapshot: &ProcSnapshot) -> String {
    let idle_us: u64 = snapshot
        .cpus
        .iter()
        .map(|cpu| {
            snapshot
                .uptime_us
                .saturating_sub(cpu.busy_us.min(snapshot.uptime_us))
        })
        .sum();
    format!(
        "{}.{:02} {}.{:02}\n",
        snapshot.uptime_us / 1_000_000,
        snapshot.uptime_us / 10_000 % 100,
        idle_us / 1_000_000,
        idle_us / 10_000 % 100
    )
}

fn format_process_stat(process: &ProcProcessSnapshot) -> String {
    let comm = String::from_utf8_lossy(&process.comm).replace(['(', ')', '\n'], "?");
    let virtual_size = process.virtual_pages.saturating_mul(4096);
    format!(
        "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 {} 0 0 0 {} {} {} 0 {} {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {}\n",
        process.pid,
        comm,
        process.state as char,
        process.ppid,
        process.process_group,
        process.session,
        ticks(process.runtime_us),
        process.priority,
        process.nice,
        process.threads,
        ticks(process.start_time_us),
        virtual_size,
        process.resident_pages,
        process.last_cpu
    )
}
