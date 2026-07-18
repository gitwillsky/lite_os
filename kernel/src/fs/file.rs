#[path = "file/character.rs"]
mod character;
#[path = "file/descriptor_table.rs"]
mod descriptor_table;
#[path = "file/proc.rs"]
mod proc;
mod terminal;
pub(crate) use character::{CharacterDevice, KmsgDeviceRead};
pub(crate) use descriptor_table::{
    CancelledFileReservation, DetachedFileDescriptor, FileDescriptorError, FileDescriptorTable,
    MAX_FILE_DESCRIPTORS,
};
pub(crate) use terminal::{Terminal, TerminalAccess, TerminalRead, TerminalReadMode};

use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;
use spin::Mutex;

use super::{
    AccessIdentity, DeviceKind, Epoll, FileSystemError, FileSystemStatistics, Inode, OpenedFile,
    vfs,
};
use crate::{
    ipc::{EventFd, PipeEnd},
    socket::{Socket, UnixNode, UnixPassedFile},
};

impl UnixPassedFile for OpenFileDescription {
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn unix_node(&self) -> Option<UnixNode> {
        match &self.kind {
            OpenFileKind::Socket(socket) => socket.unix_node(),
            _ => None,
        }
    }

    fn externally_referenced(self: Arc<Self>, inflight: usize) -> bool {
        // 1. graph 每条 outgoing edge 持有一个 OFD Arc。
        // 2. Weak::upgrade 为本次 probe 临时增加一个 Arc。
        // 3. 超过两者的引用才是 descriptor/active syscall 等外部 root；漏掉 +1 会误保活 cycle。
        Arc::strong_count(&self) > inflight.saturating_add(1)
    }
}

pub(crate) const O_ACCMODE: u32 = 3;
pub(crate) const O_RDONLY: u32 = 0;
pub(crate) const O_WRONLY: u32 = 1;
pub(crate) const O_RDWR: u32 = 2;
pub(crate) const O_APPEND: u32 = 0x400;
pub(crate) const O_NONBLOCK: u32 = 0x800;
pub(crate) const O_CLOEXEC: u32 = 0x80000;

/// @description OFD 后端；character device、pipe 和 inode 共享同一 fd 表。
pub(crate) enum OpenFileKind {
    Character(CharacterDevice),
    Pipe(Arc<PipeEnd>),
    Socket(Arc<Socket>),
    Epoll(Arc<Epoll>),
    EventFd(Arc<EventFd>),
    Inode(Arc<OpenedFile>),
}

/// @description console 文件后端 seam；具体 platform adapter 只在 composition root 装配。
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
    character_opened: Option<Arc<OpenedFile>>,
    // fork 后各 fd table 使用独立锁，单表扫描无法识别最后一个 descriptor；该计数负责跨表触发
    // epoll 的 Linux close cleanup，缺失时会留下 fd reuse 可命中的旧 interest。
    descriptor_refs: AtomicUsize,
}

impl OpenFileDescription {
    /// @description 从唯一 OFD backend 投影 poll/epoll readiness，不注册 waiter。
    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        const READ_HANGUP: i16 = 0x2000;
        let mut result = 0;
        match &self.kind {
            OpenFileKind::Inode(_) => result = events & (INPUT | OUTPUT),
            OpenFileKind::Character(device) => result = device.poll_events(events),
            OpenFileKind::Pipe(endpoint) => {
                let state = endpoint.pipe().poll_state(endpoint.direction());
                if events & INPUT != 0 && state.readable {
                    result |= INPUT;
                }
                if events & OUTPUT != 0 && state.writable {
                    result |= OUTPUT;
                }
                if state.error {
                    result |= ERROR;
                }
                if state.hangup {
                    result |= HANGUP;
                }
            }
            OpenFileKind::Socket(socket) => {
                let state = socket.poll_state();
                if events & INPUT != 0 && state.readable {
                    result |= INPUT;
                }
                if events & OUTPUT != 0 && state.writable {
                    result |= OUTPUT;
                }
                if state.error {
                    result |= ERROR;
                }
                if state.hangup {
                    result |= HANGUP;
                    if events & READ_HANGUP != 0 {
                        result |= READ_HANGUP;
                    }
                }
            }
            OpenFileKind::Epoll(epoll) => {
                if events & INPUT != 0 && epoll.has_ready() {
                    result |= INPUT;
                }
            }
            OpenFileKind::EventFd(event) => {
                if events & INPUT != 0 && event.readable() {
                    result |= INPUT;
                }
                if events & OUTPUT != 0 && event.writable() {
                    result |= OUTPUT;
                }
            }
        }
        result
    }

    /// @description 返回当前 OFD 最近一次可观察 I/O 状态变化的全局 generation。
    ///
    /// @param events caller 关注的 poll event mask。
    /// @return 跨 source 可比较的 generation；不支持 epoll 的 inode/device 返回零。
    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        match &self.kind {
            OpenFileKind::Character(device) => device.readiness_generation(),
            OpenFileKind::Pipe(endpoint) => {
                endpoint.pipe().readiness_generation(endpoint.direction())
            }
            OpenFileKind::Socket(socket) => socket.readiness_generation(events),
            OpenFileKind::Epoll(epoll) => epoll.readiness_generation(),
            OpenFileKind::EventFd(event) => event.readiness_generation(events),
            OpenFileKind::Inode(_) => 0,
        }
    }

    /// @description 判断 backend 是否提供可注册 wait source，而非仅提供同步 poll 结果。
    ///
    /// @return 可加入 epoll 返回 true；regular inode/null/zero 返回 false 并映射 EPERM。
    pub(crate) fn epoll_pollable(&self) -> bool {
        match &self.kind {
            OpenFileKind::Character(device) => device.epoll_pollable(),
            OpenFileKind::Pipe(_)
            | OpenFileKind::Socket(_)
            | OpenFileKind::Epoll(_)
            | OpenFileKind::EventFd(_) => true,
            OpenFileKind::Inode(_) => false,
        }
    }

    /// @description 构造继承给 init 的 console OFD，并保留 devfs opened entry。
    ///
    /// @param terminal 共享 TTY owner。
    /// @param backing_opened `/dev/console` opened entry，用于 metadata、fstatfs 与 procfs。
    /// @param flags OFD status flags。
    /// @return 新 console OFD。
    pub(crate) fn terminal(
        terminal: Arc<Terminal>,
        backing_opened: Arc<OpenedFile>,
        flags: u32,
    ) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::Character(CharacterDevice::Terminal {
                terminal,
                kind: DeviceKind::Console,
                pty: None,
            }),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: Some(backing_opened),
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| ())
    }

    /// @description 构造 pathname 打开的 character-device OFD。
    ///
    /// @param kind device identity。
    /// @param terminal 共享 TTY owner。
    /// @param flags OFD status flags。
    /// @param backing_opened 打开时的 devfs opened entry，用于 metadata、fstatfs 与 procfs。
    /// @return 新 character-device OFD。
    pub(crate) fn character(
        kind: DeviceKind,
        terminal: Arc<Terminal>,
        identity: &AccessIdentity,
        flags: u32,
        backing_opened: Arc<OpenedFile>,
    ) -> Result<Arc<Self>, FileSystemError> {
        let device = CharacterDevice::open(kind, terminal, identity)?;
        Arc::try_new(Self {
            kind: OpenFileKind::Character(device),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: Some(backing_opened),
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(crate) fn inode(opened: Arc<OpenedFile>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::Inode(opened),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| ())
    }

    pub(crate) fn pipe(endpoint: Arc<PipeEnd>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::Pipe(endpoint),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| ())
    }

    pub(crate) fn socket(socket: Arc<Socket>, flags: u32) -> Result<Arc<Self>, ()> {
        let ofd = Arc::try_new(Self {
            kind: OpenFileKind::Socket(socket.clone()),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| ())?;
        let owner: Arc<dyn UnixPassedFile> = ofd.clone();
        socket.bind_unix_rights_owner(Arc::downgrade(&owner));
        Ok(ofd)
    }

    pub(crate) fn epoll(epoll: Arc<Epoll>) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::Epoll(epoll),
            offset: Mutex::new(0),
            flags: Mutex::new(O_RDWR),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| ())
    }

    pub(crate) fn event_fd(event: Arc<EventFd>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::EventFd(event),
            offset: Mutex::new(0),
            flags: Mutex::new(O_RDWR | flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
        .map_err(|_| ())
    }

    pub(crate) fn inode_ref(&self) -> Option<Arc<dyn Inode>> {
        match &self.kind {
            OpenFileKind::Inode(opened) => Some(opened.inode()),
            OpenFileKind::Character(_) => None,
            OpenFileKind::Pipe(_)
            | OpenFileKind::Socket(_)
            | OpenFileKind::Epoll(_)
            | OpenFileKind::EventFd(_) => None,
        }
    }

    /// @description 返回 pathname-backed OFD 的稳定 opened-entry identity。
    /// @return regular/directory/character OFD 返回 opened entry；anonymous OFD 返回 None。
    pub(crate) fn opened_ref(&self) -> Option<Arc<OpenedFile>> {
        match &self.kind {
            OpenFileKind::Inode(opened) => Some(opened.clone()),
            OpenFileKind::Character(_) => self.character_opened.clone(),
            OpenFileKind::Pipe(_)
            | OpenFileKind::Socket(_)
            | OpenFileKind::Epoll(_)
            | OpenFileKind::EventFd(_) => None,
        }
    }

    /// @description 取得该 OFD backing filesystem 的统计；anonymous pipe 使用 pipefs 语义。
    ///
    /// @return mounted inode 的 VFS 快照，或 Linux simple_statfs 形状的 pipefs 快照。
    /// @errors 无 backing filesystem 的 OFD 返回 `InvalidFileSystem`。
    pub(crate) fn filesystem_statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        match &self.kind {
            OpenFileKind::Inode(opened) => vfs().statistics(opened.inode()),
            OpenFileKind::Character(_) => vfs().statistics(
                self.character_opened
                    .clone()
                    .ok_or(FileSystemError::InvalidFileSystem)?
                    .inode(),
            ),
            OpenFileKind::Pipe(_) | OpenFileKind::Socket(_) => Ok(FileSystemStatistics {
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
            OpenFileKind::Epoll(_) | OpenFileKind::EventFd(_) => {
                Err(FileSystemError::InvalidFileSystem)
            }
        }
    }
}
