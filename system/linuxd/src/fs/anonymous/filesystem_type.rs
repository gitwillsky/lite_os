//! 内部 pseudo filesystem 的规范类型、保留身份与 statfs 投影。
use super::super::FileSystemStatistics;

/// 内部 filesystem 身份独立于可挂载实例；两个 statfs 入口读取同一事实。
#[derive(Clone, Copy)]
pub(in crate::fs) enum PseudoFileSystem {
    Memory,
    Pipe,
    Socket,
    Anonymous,
    Namespace,
    Pid,
}
impl PseudoFileSystem {
    pub(in crate::fs) const fn id(self) -> usize {
        match self {
            Self::Memory => 6,
            Self::Pipe => 8,
            Self::Socket => 9,
            Self::Anonymous => 10,
            Self::Namespace => 11,
            Self::Pid => 12,
        }
    }
    pub(in crate::fs) const fn type_name(self) -> &'static str {
        match self {
            Self::Memory => "tmpfs",
            Self::Pipe => "pipefs",
            Self::Socket => "sockfs",
            Self::Anonymous => "anon_inodefs",
            Self::Namespace => "nsfs",
            Self::Pid => "pidfs",
        }
    }
    pub(in crate::fs) fn statistics(self) -> FileSystemStatistics {
        let magic = match self {
            Self::Memory => 0x0102_1994,
            Self::Pipe => 0x5049_5045,
            Self::Socket => 0x534f_434b,
            Self::Anonymous => 0x0904_1934,
            Self::Namespace => 0x6e73_6673,
            Self::Pid => 0x5049_4446,
        };
        FileSystemStatistics {
            magic,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [self.id() as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 0x20,
        }
    }
}
