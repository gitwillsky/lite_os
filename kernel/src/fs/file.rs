use alloc::{sync::Arc, vec, vec::Vec};
use spin::Mutex;

use super::{FileSystemError, Inode};

pub(crate) const O_ACCMODE: u32 = 3;
pub(crate) const O_RDONLY: u32 = 0;
pub(crate) const O_WRONLY: u32 = 1;
pub(crate) const O_APPEND: u32 = 0x400;
pub(crate) const O_CLOEXEC: u32 = 0x80000;
pub(crate) const MAX_FILE_DESCRIPTORS: usize = 1024;

/// @description OFD 后端；console 和 inode 共享同一 fd 表，不保留 syscall 特判旁路。
pub(crate) enum OpenFileKind {
    Console(Arc<dyn Console>),
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
}

impl OpenFileDescription {
    pub(crate) fn console(console: Arc<dyn Console>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Console(console),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
        })
    }

    pub(crate) fn inode(inode: Arc<dyn Inode>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Inode(inode),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
        })
    }

    pub(crate) fn inode_ref(&self) -> Option<Arc<dyn Inode>> {
        match &self.kind {
            OpenFileKind::Inode(inode) => Some(inode.clone()),
            OpenFileKind::Console(_) => None,
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

    pub(crate) fn with_console(console: Arc<dyn Console>) -> Self {
        Self {
            entries: vec![
                Some(FileDescriptor {
                    ofd: OpenFileDescription::console(console.clone(), O_RDONLY),
                    cloexec: false,
                }),
                Some(FileDescriptor {
                    ofd: OpenFileDescription::console(console.clone(), O_WRONLY),
                    cloexec: false,
                }),
                Some(FileDescriptor {
                    ofd: OpenFileDescription::console(console, O_WRONLY),
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

    pub(crate) fn close(&mut self, fd: usize) -> Result<(), ()> {
        let entry = self.entries.get_mut(fd).ok_or(())?;
        if entry.take().is_none() {
            return Err(());
        }
        Ok(())
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
