#[path = "file/proc.rs"]
mod proc;
mod terminal;
pub(crate) use terminal::{Terminal, TerminalAccess, TerminalRead};

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering, fence};
use spin::Mutex;

use super::Epoll;
use super::{DeviceKind, FileSystemError, FileSystemStatistics, Inode, OpenedFile, vfs};
use crate::{
    ipc::{EventFd, PipeEnd},
    socket::Socket,
};

pub(crate) const O_ACCMODE: u32 = 3;
pub(crate) const O_RDONLY: u32 = 0;
pub(crate) const O_WRONLY: u32 = 1;
pub(crate) const O_RDWR: u32 = 2;
pub(crate) const O_APPEND: u32 = 0x400;
pub(crate) const O_NONBLOCK: u32 = 0x800;
pub(crate) const O_CLOEXEC: u32 = 0x80000;
pub(crate) const MAX_FILE_DESCRIPTORS: usize = 1_048_576;

/// @description 标准 character-device OFD backend；设备 identity 与运行时 owner 保持在一起。
pub(crate) enum CharacterDevice {
    Null,
    Zero,
    Entropy(DeviceKind),
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
            Self::Entropy(kind) => *kind,
            Self::Terminal { kind, .. } => *kind,
        }
    }
}

/// @description OFD 后端；character device、pipe 和 inode 共享同一 fd 表。
pub(crate) enum OpenFileKind {
    Character(CharacterDevice),
    Pipe(Arc<PipeEnd>),
    Socket(Arc<Socket>),
    Epoll(Arc<Epoll>),
    EventFd(Arc<EventFd>),
    Inode(Arc<OpenedFile>),
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
            OpenFileKind::Character(device) => match device {
                CharacterDevice::Null | CharacterDevice::Zero => result = events & (INPUT | OUTPUT),
                CharacterDevice::Entropy(_) => result = events & INPUT,
                CharacterDevice::Terminal { terminal, .. } => {
                    result = events & OUTPUT;
                    if events & INPUT != 0 && terminal.wait_ready() {
                        result |= INPUT;
                    }
                }
            },
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
            OpenFileKind::Character(CharacterDevice::Terminal { terminal, .. }) => {
                terminal.readiness_generation()
            }
            OpenFileKind::Pipe(endpoint) => {
                endpoint.pipe().readiness_generation(endpoint.direction())
            }
            OpenFileKind::Socket(socket) => socket.readiness_generation(events),
            OpenFileKind::Epoll(epoll) => epoll.readiness_generation(),
            OpenFileKind::EventFd(event) => event.readiness_generation(events),
            OpenFileKind::Character(
                CharacterDevice::Null | CharacterDevice::Zero | CharacterDevice::Entropy(_),
            )
            | OpenFileKind::Inode(_) => 0,
        }
    }

    /// @description 判断 backend 是否提供可注册 wait source，而非仅提供同步 poll 结果。
    ///
    /// @return 可加入 epoll 返回 true；regular inode/null/zero 返回 false 并映射 EPERM。
    pub(crate) fn epoll_pollable(&self) -> bool {
        matches!(
            self.kind,
            OpenFileKind::Character(CharacterDevice::Terminal { .. })
                | OpenFileKind::Pipe(_)
                | OpenFileKind::Socket(_)
                | OpenFileKind::Epoll(_)
                | OpenFileKind::EventFd(_)
        )
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
    ) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Character(CharacterDevice::Terminal {
                terminal,
                kind: DeviceKind::Console,
            }),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: Some(backing_opened),
            descriptor_refs: AtomicUsize::new(0),
        })
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
        flags: u32,
        backing_opened: Arc<OpenedFile>,
    ) -> Arc<Self> {
        let device = match kind {
            DeviceKind::Null => CharacterDevice::Null,
            DeviceKind::Zero => CharacterDevice::Zero,
            DeviceKind::Random | DeviceKind::Urandom => CharacterDevice::Entropy(kind),
            DeviceKind::Tty | DeviceKind::Console => CharacterDevice::Terminal { terminal, kind },
        };
        Arc::new(Self {
            kind: OpenFileKind::Character(device),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: Some(backing_opened),
            descriptor_refs: AtomicUsize::new(0),
        })
    }

    pub(crate) fn inode(opened: Arc<OpenedFile>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Inode(opened),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
    }

    pub(crate) fn pipe(endpoint: Arc<PipeEnd>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Pipe(endpoint),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
    }

    pub(crate) fn socket(socket: Arc<Socket>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Socket(socket),
            offset: Mutex::new(0),
            flags: Mutex::new(flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
    }

    pub(crate) fn epoll(epoll: Arc<Epoll>) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::Epoll(epoll),
            offset: Mutex::new(0),
            flags: Mutex::new(O_RDWR),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
    }

    pub(crate) fn event_fd(event: Arc<EventFd>, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind: OpenFileKind::EventFd(event),
            offset: Mutex::new(0),
            flags: Mutex::new(O_RDWR | flags),
            character_opened: None,
            descriptor_refs: AtomicUsize::new(0),
        })
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

struct FileDescriptor {
    ofd: Arc<OpenFileDescription>,
    cloexec: bool,
}

impl FileDescriptor {
    fn new(ofd: Arc<OpenFileDescription>, cloexec: bool) -> Self {
        // fd table lock/Process publication owns entry visibility；该原子只计数，不发布 OFD 数据，
        // 因此 increment 使用 Relaxed。缺少 increment 会让任一 close 提前删除仍存活的 interest。
        ofd.descriptor_refs.fetch_add(1, Ordering::Relaxed);
        Self { ofd, cloexec }
    }
}

impl Clone for FileDescriptor {
    fn clone(&self) -> Self {
        Self::new(self.ofd.clone(), self.cloexec)
    }
}

impl Drop for FileDescriptor {
    fn drop(&mut self) {
        // Release/Acquire 与其他 fd table 的最后 decrement 配对，确保判定为最后引用后才执行
        // 全局 cleanup；缺少原子 RMW 会让 fork 后两个 table 同时误判生命周期。
        if self.ofd.descriptor_refs.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            Epoll::release_file(&self.ofd);
            vfs().release_advisory_lock(&self.ofd);
        }
    }
}

/// @description 进程 fd table；dup 复制 fd entry 并共享同一个 OFD。
pub(crate) struct FileDescriptorTable {
    entries: Vec<Option<FileDescriptor>>,
}

impl FileDescriptorTable {
    fn ensure_len(&mut self, length: usize) -> Result<(), ()> {
        if length <= self.entries.len() {
            return Ok(());
        }
        self.entries
            .try_reserve_exact(length - self.entries.len())
            .map_err(|_| ())?;
        self.entries.resize(length, None);
        Ok(())
    }

    /// @description 返回当前 fd table 已分配的 descriptor slot 数。
    /// @return 包含空洞的 slot 容量，对应 Linux `/proc/<pid>/status` FDSize。
    pub(crate) fn slot_capacity(&self) -> usize {
        self.entries.len()
    }

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
    /// @param terminal 唯一 TTY owner；backing opened entry 从已挂载 devfs 解析一次。
    /// @return fd 0/1/2 分别为 console read/write/write OFD 的 descriptor table。
    pub(crate) fn with_terminal(terminal: Arc<Terminal>) -> Self {
        let backing_opened = vfs()
            .open_file(b"/dev/console")
            .expect("mounted console device must resolve");
        Self {
            entries: vec![
                Some(FileDescriptor::new(
                    OpenFileDescription::terminal(
                        terminal.clone(),
                        backing_opened.clone(),
                        O_RDONLY,
                    ),
                    false,
                )),
                Some(FileDescriptor::new(
                    OpenFileDescription::terminal(
                        terminal.clone(),
                        backing_opened.clone(),
                        O_WRONLY,
                    ),
                    false,
                )),
                Some(FileDescriptor::new(
                    OpenFileDescription::terminal(terminal, backing_opened, O_WRONLY),
                    false,
                )),
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
        limit: usize,
    ) -> Result<usize, ()> {
        let limit = limit.min(MAX_FILE_DESCRIPTORS);
        if minimum >= limit {
            return Err(());
        }
        for fd in minimum..self.entries.len().min(limit) {
            if self.entries[fd].is_none() {
                self.entries[fd] = Some(FileDescriptor::new(ofd, cloexec));
                return Ok(fd);
            }
        }
        if self.entries.len() < minimum {
            self.ensure_len(minimum)?;
        }
        let fd = self.entries.len();
        if fd >= limit {
            return Err(());
        }
        self.entries.push(Some(FileDescriptor::new(ofd, cloexec)));
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
        limit: usize,
    ) -> Result<(usize, usize), ()> {
        let mut available = [usize::MAX; 2];
        let mut found = 0;
        for fd in 0..limit.min(MAX_FILE_DESCRIPTORS) {
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
        self.ensure_len(required)?;
        self.entries[available[0]] = Some(FileDescriptor::new(first, cloexec));
        self.entries[available[1]] = Some(FileDescriptor::new(second, cloexec));
        Ok((available[0], available[1]))
    }

    pub(crate) fn close(&mut self, fd: usize) -> Result<(), ()> {
        let entry = self.entries.get_mut(fd).ok_or(())?;
        drop(entry.take().ok_or(())?);
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
        limit: usize,
    ) -> Result<usize, ()> {
        let ofd = self.get(old).ok_or(())?;
        self.allocate(ofd, minimum, cloexec, limit)
    }

    pub(crate) fn duplicate_to(
        &mut self,
        old: usize,
        new: usize,
        cloexec: bool,
        limit: usize,
    ) -> Result<usize, ()> {
        if new >= limit.min(MAX_FILE_DESCRIPTORS) {
            return Err(());
        }
        let ofd = self.get(old).ok_or(())?;
        if self.entries.len() <= new {
            self.ensure_len(new + 1)?;
        }
        drop(self.entries[new].replace(FileDescriptor::new(ofd, cloexec)));
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

    /// @description 取走一个 FD_CLOEXEC entry，让 Process owner 在 files lock 外执行 close cleanup。
    ///
    /// @return 被关闭 entry 的 OFD；不存在更多 CLOEXEC entry 时返回 None。
    pub(crate) fn take_cloexec(&mut self) -> Option<Arc<OpenFileDescription>> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.as_ref().is_some_and(|entry| entry.cloexec))?
            .take()
            .expect("matched cloexec entry disappeared");
        let ofd = entry.ofd.clone();
        drop(entry);
        Some(ofd)
    }
}
