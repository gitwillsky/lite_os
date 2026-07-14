use super::InodeType;

#[derive(Clone, Copy)]
pub(super) enum ProcNode {
    Root,
    Stat,
    MemInfo,
    LoadAvg,
    Uptime,
    Mounts,
    NetDir,
    NetDev,
    NetRoute,
    SelfLink,
    ProcessDir(usize),
    ProcessStat(usize),
    ProcessStatus(usize),
    ProcessCmdline(usize),
    ProcessComm(usize),
    ProcessStatm(usize),
    ProcessTaskDir(usize),
    ProcessFdDir(usize),
    ProcessFd(usize, usize),
    ThreadDir(usize, usize),
    ThreadStat(usize, usize),
    ThreadStatus(usize, usize),
    ThreadCmdline(usize, usize),
    ThreadComm(usize, usize),
    ThreadStatm(usize, usize),
}

impl ProcNode {
    pub(super) fn inode(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Stat => 2,
            Self::MemInfo => 3,
            Self::LoadAvg => 4,
            Self::Uptime => 5,
            Self::Mounts => 6,
            Self::NetDir => 7,
            Self::NetDev => 8,
            Self::NetRoute => 9,
            Self::SelfLink => 10,
            Self::ProcessDir(pid) => 0x1000_0000_0000_0000 | (pid as u64) << 4,
            Self::ProcessStat(pid) => 0x1000_0000_0000_0001 | (pid as u64) << 4,
            Self::ProcessStatus(pid) => 0x1000_0000_0000_0002 | (pid as u64) << 4,
            Self::ProcessCmdline(pid) => 0x1000_0000_0000_0003 | (pid as u64) << 4,
            Self::ProcessComm(pid) => 0x1000_0000_0000_0004 | (pid as u64) << 4,
            Self::ProcessFdDir(pid) => 0x1000_0000_0000_0005 | (pid as u64) << 4,
            Self::ProcessStatm(pid) => 0x1000_0000_0000_0006 | (pid as u64) << 4,
            Self::ProcessTaskDir(pid) => 0x1000_0000_0000_0007 | (pid as u64) << 4,
            Self::ProcessFd(pid, fd) => 0x2000_0000_0000_0000 | (pid as u64) << 10 | fd as u64,
            Self::ThreadDir(_, tid) => 0x3000_0000_0000_0000 | (tid as u64) << 4,
            Self::ThreadStat(_, tid) => 0x3000_0000_0000_0001 | (tid as u64) << 4,
            Self::ThreadStatus(_, tid) => 0x3000_0000_0000_0002 | (tid as u64) << 4,
            Self::ThreadCmdline(_, tid) => 0x3000_0000_0000_0003 | (tid as u64) << 4,
            Self::ThreadComm(_, tid) => 0x3000_0000_0000_0004 | (tid as u64) << 4,
            Self::ThreadStatm(_, tid) => 0x3000_0000_0000_0005 | (tid as u64) << 4,
        }
    }

    pub(super) fn kind(self) -> InodeType {
        match self {
            Self::Root
            | Self::NetDir
            | Self::ProcessDir(_)
            | Self::ProcessTaskDir(_)
            | Self::ProcessFdDir(_)
            | Self::ThreadDir(_, _) => InodeType::Directory,
            Self::SelfLink | Self::ProcessFd(_, _) => InodeType::SymLink,
            _ => InodeType::File,
        }
    }
}
