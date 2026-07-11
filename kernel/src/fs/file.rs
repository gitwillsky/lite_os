use alloc::{sync::Arc, vec, vec::Vec};
use spin::Mutex;

use super::Inode;

pub const O_ACCMODE: u32 = 3;
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_APPEND: u32 = 0x400;
pub const O_CLOEXEC: u32 = 0x80000;
pub const MAX_FILE_DESCRIPTORS: usize = 1024;

/// @description OFD 后端；console 和 inode 共享同一 fd 表，不保留 syscall 特判旁路。
pub enum OpenFileKind {
    Console,
    Inode(Arc<dyn Inode>),
}

/// @description Linux open file description，共享偏移和状态标志。
pub struct OpenFileDescription {
    pub kind: OpenFileKind,
    pub offset: Mutex<u64>,
    pub flags: Mutex<u32>,
}

impl OpenFileDescription {
    pub fn console(flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Console,
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
        })
    }

    pub fn inode(inode: Arc<dyn Inode>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Inode(inode),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
        })
    }

    pub fn inode_ref(&self) -> Option<Arc<dyn Inode>> {
        match &self.kind {
            OpenFileKind::Inode(inode) => Some(inode.clone()),
            OpenFileKind::Console => None,
        }
    }
}

#[derive(Clone)]
struct FileDescriptor {
    ofd: Arc<OpenFileDescription>,
    cloexec: bool,
}

/// @description 进程 fd table；dup 复制 fd entry 并共享同一个 OFD。
pub struct FileDescriptorTable {
    entries: Vec<Option<FileDescriptor>>,
}

impl FileDescriptorTable {
    pub fn with_console() -> Self {
        Self {
            entries: vec![
                Some(FileDescriptor {
                    ofd: OpenFileDescription::console(O_RDONLY),
                    cloexec: false,
                }),
                Some(FileDescriptor {
                    ofd: OpenFileDescription::console(O_WRONLY),
                    cloexec: false,
                }),
                Some(FileDescriptor {
                    ofd: OpenFileDescription::console(O_WRONLY),
                    cloexec: false,
                }),
            ],
        }
    }

    pub fn get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.entries
            .get(fd)?
            .as_ref()
            .map(|entry| entry.ofd.clone())
    }

    pub fn allocate(
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

    pub fn close(&mut self, fd: usize) -> Result<(), ()> {
        let entry = self.entries.get_mut(fd).ok_or(())?;
        if entry.take().is_none() {
            return Err(());
        }
        Ok(())
    }

    pub fn duplicate(&mut self, old: usize, minimum: usize, cloexec: bool) -> Result<usize, ()> {
        let ofd = self.get(old).ok_or(())?;
        self.allocate(ofd, minimum, cloexec)
    }

    pub fn duplicate_to(&mut self, old: usize, new: usize, cloexec: bool) -> Result<usize, ()> {
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

    pub fn descriptor_flags(&self, fd: usize) -> Result<u32, ()> {
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

    pub fn set_descriptor_flags(&mut self, fd: usize, flags: u32) -> Result<(), ()> {
        self.entries
            .get_mut(fd)
            .and_then(Option::as_mut)
            .ok_or(())?
            .cloexec = flags & 1 != 0;
        Ok(())
    }

    pub fn close_cloexec(&mut self) {
        for entry in &mut self.entries {
            if entry.as_ref().is_some_and(|entry| entry.cloexec) {
                *entry = None;
            }
        }
    }
}
