use super::InodeType;
use crate::namespace::{NamespaceSelector, UserNamespaceMap};

/// 直接 /proc/TID 与 /proc/TGID/task/TID 的目录父入口；资源仍按同一实际 Thread 读取。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ProcThreadDirectory {
    Root,
    Group,
}
impl ProcThreadDirectory {
    pub(super) fn parent(self, tgid: usize) -> ProcNode {
        match self {
            Self::Root => ProcNode::Root,
            Self::Group => ProcNode::ProcessTaskDir(tgid),
        }
    }
}

/// fd/ns 子目录的真实父入口；task/TGID 仍是 Thread 目录，不能凭 tgid==tid 改成 Process 目录。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ProcTaskDirectory {
    Process,
    Thread(ProcThreadDirectory),
}
impl ProcTaskDirectory {
    pub(super) fn parent(self, tgid: usize, tid: usize) -> ProcNode {
        match self {
            Self::Process => ProcNode::ProcessDir(tgid),
            Self::Thread(directory) => ProcNode::ThreadDir(tgid, tid, directory),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ProcNode {
    Root,
    SysDir,
    NetSysctlDir,
    CoreNetSysctlDir,
    CoreBufferLimit(crate::socket::CoreBufferLimit),
    OptmemLimit,
    SysvipcDir,
    IpcTable(crate::ipc::IpcTable),
    FsSysctlDir,
    InotifySysctlDir,
    InotifyLimit(crate::fs::InotifyLimit),
    KernelSysctlDir,
    PidLimit,
    IpcLimit(crate::ipc::IpcLimit),
    MountLimit,
    UserSysctlDir,
    UserResourceLimit(crate::namespace::UserResource),
    Stat,
    MemInfo,
    Swaps,
    BuddyInfo,
    VmStat,
    LoadAvg,
    Uptime,
    Mounts,
    Partitions,
    Cmdline,
    NetLink,
    TaskNetDir(usize, usize, ProcTaskDirectory),
    NetDev,
    NetRoute,
    SelfLink,
    ProcessDir(usize),
    ProcessStat(usize),
    ProcessStatus(usize),
    ProcessCmdline(usize),
    ProcessComm(usize),
    ProcessStatm(usize),
    ProcessIo(usize),
    ProcessMounts(usize),
    ProcessMountInfo(usize),
    ProcessMountStats(usize),
    TaskExe(usize),
    ProcessRoot(usize),
    ProcessCwd(usize),
    ProcessTaskDir(usize),
    TaskFdDir(usize, usize, ProcTaskDirectory),
    TaskFd(usize, usize),
    TaskFdInfoDir(usize, usize, ProcTaskDirectory),
    TaskFdInfo(usize, usize),
    ThreadDir(usize, usize, ProcThreadDirectory),
    ThreadStat(usize, usize),
    ThreadStatus(usize, usize),
    ThreadCmdline(usize, usize),
    ThreadComm(usize, usize),
    ThreadStatm(usize, usize),
    ThreadIo(usize, usize),
    ThreadMounts(usize),
    ThreadMountInfo(usize),
    ThreadMountStats(usize),
    ThreadRoot(usize),
    ThreadCwd(usize),
    NamespaceDir(usize, usize, ProcTaskDirectory),
    NamespaceLink(usize, NamespaceSelector),
    UserMap(usize, UserNamespaceMap),
    TimeOffsets(usize),
}

impl ProcNode {
    /// 动态内容的真实 TGID；thread 文件显式携带 group key，不扫描全部进程。
    pub(super) fn process_key(self) -> Option<usize> {
        match self {
            Self::ThreadDir(tgid, _, _)
            | Self::ThreadStat(tgid, _)
            | Self::ThreadStatus(tgid, _)
            | Self::ThreadCmdline(tgid, _)
            | Self::ThreadComm(tgid, _)
            | Self::ThreadStatm(tgid, _)
            | Self::ThreadIo(tgid, _) => Some(tgid),
            _ => self.owner_tid(),
        }
    }

    /// PID/thread 两个目录入口共享实际 mount table 操作与权限事实。
    pub(super) fn mount_table(self) -> Option<(usize, crate::fs::MountTableKind)> {
        use crate::fs::MountTableKind;
        match self {
            Self::ProcessMounts(tid) | Self::ThreadMounts(tid) => {
                Some((tid, MountTableKind::Mounts))
            }
            Self::ProcessMountInfo(tid) | Self::ThreadMountInfo(tid) => {
                Some((tid, MountTableKind::Info))
            }
            Self::ProcessMountStats(tid) | Self::ThreadMountStats(tid) => {
                Some((tid, MountTableKind::Stats))
            }
            _ => None,
        }
    }

    pub(super) const GENERIC_ENTRY_COUNT: usize = 21;
    pub(super) fn generic_entries() -> [Self; Self::GENERIC_ENTRY_COUNT] {
        [
            Self::Root,
            Self::Stat,
            Self::MemInfo,
            Self::LoadAvg,
            Self::Uptime,
            Self::Mounts,
            Self::NetLink,
            Self::SelfLink,
            Self::BuddyInfo,
            Self::VmStat,
            Self::Cmdline,
            Self::Partitions,
            Self::Swaps,
            Self::SysDir,
            Self::FsSysctlDir,
            Self::InotifySysctlDir,
            Self::MountLimit,
            Self::SysvipcDir,
            Self::IpcTable(crate::ipc::IpcTable::Message),
            Self::IpcTable(crate::ipc::IpcTable::Semaphore),
            Self::IpcTable(crate::ipc::IpcTable::SharedMemory),
        ]
    }
    pub(super) fn generic_index(self) -> Option<usize> {
        Self::generic_entries()
            .iter()
            .position(|node| *node == self)
    }

    /// PID inode 的 live owner 来自绑定的实际 Thread，不能冻结创建时凭据。
    pub(super) fn owner_tid(self) -> Option<usize> {
        match self {
            Self::ProcessDir(tid)
            | Self::ProcessStat(tid)
            | Self::ProcessStatus(tid)
            | Self::ProcessCmdline(tid)
            | Self::ProcessComm(tid)
            | Self::ProcessStatm(tid)
            | Self::ProcessIo(tid)
            | Self::ProcessMounts(tid)
            | Self::ProcessMountInfo(tid)
            | Self::ProcessMountStats(tid)
            | Self::ThreadMounts(tid)
            | Self::ThreadMountInfo(tid)
            | Self::ThreadMountStats(tid)
            | Self::TaskExe(tid)
            | Self::ProcessRoot(tid)
            | Self::ProcessCwd(tid)
            | Self::ProcessTaskDir(tid)
            | Self::TaskFd(tid, _)
            | Self::TaskFdInfo(tid, _)
            | Self::ThreadRoot(tid)
            | Self::ThreadCwd(tid)
            | Self::NamespaceLink(tid, _)
            | Self::UserMap(tid, _)
            | Self::TimeOffsets(tid) => Some(tid),
            Self::TaskFdDir(_, tid, _)
            | Self::TaskFdInfoDir(_, tid, _)
            | Self::ThreadDir(_, tid, _)
            | Self::ThreadStat(_, tid)
            | Self::ThreadStatus(_, tid)
            | Self::ThreadCmdline(_, tid)
            | Self::ThreadComm(_, tid)
            | Self::ThreadStatm(_, tid)
            | Self::ThreadIo(_, tid)
            | Self::NamespaceDir(_, tid, _)
            | Self::TaskNetDir(_, tid, _) => Some(tid),
            _ => None,
        }
    }

    /// proc 的文件种类和初始 mode；后续 mutation 由实际 inode attribute owner 授权。
    pub(super) fn metadata_mode(self) -> u32 {
        match self.kind() {
            InodeType::Directory => 0o040555,
            InodeType::SymLink => 0o120777,
            _ if matches!(
                self,
                Self::UserResourceLimit(_)
                    | Self::UserMap(_, _)
                    | Self::TimeOffsets(_)
                    | Self::MountLimit
                    | Self::InotifyLimit(_)
                    | Self::PidLimit
                    | Self::IpcLimit(_)
                    | Self::CoreBufferLimit(_)
                    | Self::OptmemLimit
            ) =>
            {
                0o100644
            }
            _ => 0o100444,
        }
    }
    /// 固定 Linux proc inode i_size；不为 metadata 读取动态 target 或 snapshot。
    pub(super) fn metadata_size(self) -> u64 {
        match self {
            Self::TaskFd(_, _) => 64,
            _ => 0,
        }
    }

    pub(super) fn kind(self) -> InodeType {
        match self {
            Self::Root
            | Self::SysDir
            | Self::NetSysctlDir
            | Self::CoreNetSysctlDir
            | Self::SysvipcDir
            | Self::FsSysctlDir
            | Self::InotifySysctlDir
            | Self::KernelSysctlDir
            | Self::UserSysctlDir
            | Self::TaskNetDir(_, _, _)
            | Self::ProcessDir(_)
            | Self::ProcessTaskDir(_)
            | Self::TaskFdDir(_, _, _)
            | Self::TaskFdInfoDir(_, _, _)
            | Self::ThreadDir(_, _, _)
            | Self::NamespaceDir(_, _, _) => InodeType::Directory,
            Self::SelfLink
            | Self::NetLink
            | Self::Mounts
            | Self::TaskExe(_)
            | Self::ProcessRoot(_)
            | Self::ProcessCwd(_)
            | Self::ThreadRoot(_)
            | Self::ThreadCwd(_)
            | Self::TaskFd(_, _)
            | Self::NamespaceLink(_, _) => InodeType::SymLink,
            _ => InodeType::File,
        }
    }
}
