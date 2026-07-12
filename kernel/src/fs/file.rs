mod terminal;
pub(crate) use terminal::{Terminal, TerminalAccess, TerminalRead};

use alloc::{sync::Arc, vec, vec::Vec};
use spin::Mutex;

use super::{DeviceKind, FileSystemError, FileSystemStatistics, Inode, vfs};
use crate::ipc::PipeEnd;

pub(crate) const O_ACCMODE: u32 = 3;
pub(crate) const O_RDONLY: u32 = 0;
pub(crate) const O_WRONLY: u32 = 1;
pub(crate) const O_APPEND: u32 = 0x400;
pub(crate) const O_NONBLOCK: u32 = 0x800;
pub(crate) const O_CLOEXEC: u32 = 0x80000;
pub(crate) const MAX_FILE_DESCRIPTORS: usize = 1024;

/// @description 标准 character-device OFD backend；设备 identity 与运行时 owner 保持在一起。
pub(crate) enum CharacterDevice {
    Null,
    Zero,
    Terminal {
        terminal: Arc<Terminal>,
        kind: DeviceKind,
    },
}

impl CharacterDevice {
    pub(crate) fn kind(&self) -> DeviceKind {
        match self {
            Self::Null => DeviceKind::Null,
            Self::Zero => DeviceKind::Zero,
            Self::Terminal { kind, .. } => *kind,
        }
    }

    pub(crate) fn terminal(&self) -> Option<&Arc<Terminal>> {
        match self {
            Self::Terminal { terminal, .. } => Some(terminal),
            Self::Null | Self::Zero => None,
        }
    }
}

/// @description OFD 后端；character device、pipe 和 inode 共享同一 fd 表。
pub(crate) enum OpenFileKind {
    Character(CharacterDevice),
    Pipe(Arc<PipeEnd>),
    Inode(Arc<dyn Inode>),
}

/// @description console 文件后端 seam；具体 SBI adapter 只在 composition root 装配。
pub(crate) trait Console: Send + Sync {
    /// @description 非阻塞读取当前 IRQ ring 中已有 console bytes。
    ///
    /// @param bytes kernel-owned 输出缓冲区。
    /// @return 已有输入长度；零表示调用方必须进入 console wait；设备失败返回 `IoError`。
    fn read(&self, bytes: &mut [u8]) -> Result<usize, FileSystemError>;

    /// @description 查询 console 是否可读，只允许在 wait owner lock 内封闭 read/enqueue race。
    fn input_ready(&self) -> bool;

    /// @description 同步写出完整或部分 console 字节流。
    ///
    /// @param bytes kernel 已完成 user-copy 的连续字节。
    /// @return 实际写出长度；底层 console 失败返回 `IoError`。
    fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError>;
}

/// @description Linux open file description，共享偏移和状态标志。
pub(crate) struct OpenFileDescription {
    pub(crate) kind: OpenFileKind,
    pub(crate) offset: Mutex<u64>,
    pub(crate) flags: Mutex<u32>,
    character_inode: Option<Arc<dyn Inode>>,
}

impl OpenFileDescription {
    /// @description 构造继承给 init 的 console OFD，并保留 devfs backing inode。
    ///
    /// @param terminal 共享 TTY owner。
    /// @param backing_inode `/dev/console` inode，用于 metadata 与 fstatfs。
    /// @param flags OFD status flags。
    /// @return 新 console OFD。
    pub(crate) fn terminal(
        terminal: Arc<Terminal>,
        backing_inode: Arc<dyn Inode>,
        flags: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Character(CharacterDevice::Terminal {
                terminal,
                kind: DeviceKind::Console,
            }),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_inode: Some(backing_inode),
        })
    }

    /// @description 构造 pathname 打开的 character-device OFD。
    ///
    /// @param kind device identity。
    /// @param terminal 共享 TTY owner。
    /// @param flags OFD status flags。
    /// @param backing_inode 打开时的 devfs inode，用于 metadata 与 fstatfs。
    /// @return 新 character-device OFD。
    pub(crate) fn character(
        kind: DeviceKind,
        terminal: Arc<Terminal>,
        flags: u32,
        backing_inode: Arc<dyn Inode>,
    ) -> Arc<Self> {
        let device = match kind {
            DeviceKind::Null => CharacterDevice::Null,
            DeviceKind::Zero => CharacterDevice::Zero,
            DeviceKind::Tty | DeviceKind::Console => CharacterDevice::Terminal { terminal, kind },
        };
        Arc::new(Self {
            kind: OpenFileKind::Character(device),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_inode: Some(backing_inode),
        })
    }

    pub(crate) fn inode(inode: Arc<dyn Inode>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Inode(inode),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_inode: None,
        })
    }

    pub(crate) fn pipe(endpoint: Arc<PipeEnd>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Pipe(endpoint),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_inode: None,
        })
    }

    pub(crate) fn inode_ref(&self) -> Option<Arc<dyn Inode>> {
        match &self.kind {
            OpenFileKind::Inode(inode) => Some(inode.clone()),
            OpenFileKind::Character(_) => None,
            OpenFileKind::Pipe(_) => None,
        }
    }

    /// @description 取得该 OFD backing filesystem 的统计；anonymous pipe 使用 pipefs 语义。
    ///
    /// @return mounted inode 的 VFS 快照，或 Linux simple_statfs 形状的 pipefs 快照。
    /// @errors 无 backing filesystem 的 OFD 返回 `InvalidFileSystem`。
    pub(crate) fn filesystem_statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        match &self.kind {
            OpenFileKind::Inode(inode) => vfs().statistics(inode.clone()),
            OpenFileKind::Character(_) => vfs().statistics(
                self.character_inode
                    .clone()
                    .ok_or(FileSystemError::InvalidFileSystem)?,
            ),
            OpenFileKind::Pipe(_) => Ok(FileSystemStatistics {
                type_name: "pipefs",
                magic: 0x5049_5045,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                files: 0,
                files_free: 0,
                fsid: [0x5049_5045, 0],
                name_length: 255,
                fragment_size: 4096,
                flags: 0x20,
            }),
        }
    }
}

#[derive(Clone)]
struct FileDescriptor {
    ofd: Arc<OpenFileDescription>,
    cloexec: bool,
}

/// @description 进程 fd table；dup 复制 fd entry 并共享同一个 OFD。
pub(crate) struct FileDescriptorTable {
    entries: Vec<Option<FileDescriptor>>,
}

impl FileDescriptorTable {
    /// @description 复制 fd entries，同时保持每个 entry 共享原 OFD Arc。
    ///
    /// @return 成功返回独立 descriptor table；kernel heap 耗尽返回错误。
    pub(crate) fn try_clone(&self) -> Result<Self, ()> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.entries.len())
            .map_err(|_| ())?;
        entries.extend(self.entries.iter().cloned());
        Ok(Self { entries })
    }

    /// @description 构造 init 的三个 inherited console descriptor。
    ///
    /// @param terminal 唯一 TTY owner；backing inode 从已挂载 devfs 解析一次。
    /// @return fd 0/1/2 分别为 console read/write/write OFD 的 descriptor table。
    pub(crate) fn with_terminal(terminal: Arc<Terminal>) -> Self {
        let backing_inode = vfs()
            .open(b"/dev/console")
            .expect("mounted console device must resolve");
        Self {
            entries: vec![
                Some(FileDescriptor {
                    ofd: OpenFileDescription::terminal(
                        terminal.clone(),
                        backing_inode.clone(),
                        O_RDONLY,
                    ),
                    cloexec: false,
                }),
                Some(FileDescriptor {
                    ofd: OpenFileDescription::terminal(
                        terminal.clone(),
                        backing_inode.clone(),
                        O_WRONLY,
                    ),
                    cloexec: false,
                }),
                Some(FileDescriptor {
                    ofd: OpenFileDescription::terminal(terminal, backing_inode, O_WRONLY),
                    cloexec: false,
                }),
            ],
        }
    }

    pub(crate) fn get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.entries
            .get(fd)?
            .as_ref()
            .map(|entry| entry.ofd.clone())
    }

    pub(crate) fn allocate(
        &mut self,
        ofd: Arc<OpenFileDescription>,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        if minimum >= MAX_FILE_DESCRIPTORS {
            return Err(());
        }
        for fd in minimum..self.entries.len() {
            if self.entries[fd].is_none() {
                self.entries[fd] = Some(FileDescriptor { ofd, cloexec });
                return Ok(fd);
            }
        }
        if self.entries.len() < minimum {
            self.entries.resize(minimum, None);
        }
        let fd = self.entries.len();
        if fd >= MAX_FILE_DESCRIPTORS {
            return Err(());
        }
        self.entries.push(Some(FileDescriptor { ofd, cloexec }));
        Ok(fd)
    }

    /// @description 原子分配 pipe read/write 两个 descriptor entry。
    ///
    /// @param first read endpoint OFD。
    /// @param second write endpoint OFD。
    /// @param cloexec 两个 descriptor 的 FD_CLOEXEC 初值。
    /// @return 两个 fd；容量不足时 fd table 不变。
    pub(crate) fn allocate_pair(
        &mut self,
        first: Arc<OpenFileDescription>,
        second: Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<(usize, usize), ()> {
        let mut available = [usize::MAX; 2];
        let mut found = 0;
        for fd in 0..MAX_FILE_DESCRIPTORS {
            if self.entries.get(fd).is_none_or(Option::is_none) {
                available[found] = fd;
                found += 1;
                if found == 2 {
                    break;
                }
            }
        }
        if found != 2 {
            return Err(());
        }
        let required = available[1] + 1;
        if self.entries.len() < required {
            self.entries.resize(required, None);
        }
        self.entries[available[0]] = Some(FileDescriptor {
            ofd: first,
            cloexec,
        });
        self.entries[available[1]] = Some(FileDescriptor {
            ofd: second,
            cloexec,
        });
        Ok((available[0], available[1]))
    }

    pub(crate) fn close(&mut self, fd: usize) -> Result<(), ()> {
        let entry = self.entries.get_mut(fd).ok_or(())?;
        entry.take().ok_or(())?;
        Ok(())
    }

    /// @description 从 live Process 原子取走全部 fd entry，供 exit 在 files lock 外关闭。
    ///
    /// @return 拥有原全部 entry 的独立 table；self 变为空 table。
    pub(crate) fn take_all(&mut self) -> Self {
        Self {
            entries: core::mem::take(&mut self.entries),
        }
    }

    pub(crate) fn duplicate(
        &mut self,
        old: usize,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        let ofd = self.get(old).ok_or(())?;
        self.allocate(ofd, minimum, cloexec)
    }

    pub(crate) fn duplicate_to(
        &mut self,
        old: usize,
        new: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        if new >= MAX_FILE_DESCRIPTORS {
            return Err(());
        }
        let ofd = self.get(old).ok_or(())?;
        if self.entries.len() <= new {
            self.entries.resize(new + 1, None);
        }
        self.entries[new] = Some(FileDescriptor { ofd, cloexec });
        Ok(new)
    }

    pub(crate) fn descriptor_flags(&self, fd: usize) -> Result<u32, ()> {
        Ok(
            if self
                .entries
                .get(fd)
                .and_then(Option::as_ref)
                .ok_or(())?
                .cloexec
            {
                1
            } else {
                0
            },
        )
    }

    pub(crate) fn set_descriptor_flags(&mut self, fd: usize, flags: u32) -> Result<(), ()> {
        self.entries
            .get_mut(fd)
            .and_then(Option::as_mut)
            .ok_or(())?
            .cloexec = flags & 1 != 0;
        Ok(())
    }

    pub(crate) fn close_cloexec(&mut self) {
        for entry in &mut self.entries {
            if entry.as_ref().is_some_and(|entry| entry.cloexec) {
                *entry = None;
            }
        }
    }
}
