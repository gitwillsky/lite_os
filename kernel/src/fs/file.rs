#[path = "file/descriptor_table.rs"]
mod descriptor_table;
#[path = "file/position.rs"]
mod position;
#[path = "file/proc.rs"]
mod proc;
mod terminal;
pub(crate) use descriptor_table::{
    CancelledFileReservation, DetachedFileDescriptor, FileDescriptorError, FileDescriptorTable,
    MAX_FILE_DESCRIPTORS,
};
pub(in crate::fs) use terminal::clear_terminal_raw_input;
pub(crate) use terminal::{
    Terminal, TerminalAccess, TerminalRead, TerminalReadMode, character_write_chunk,
};

use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;
use spin::Mutex;

use position::FilePosition;

use super::{
    BlockNode, Epoll, EpollMemberships, FileSystemError, FileSystemStatistics, Inode, InodeType,
    OpenedFile, ReadinessSource, ReadinessSources, TimerFd, vfs,
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

/// OFD 后端；character device、pipe 和 inode 共享同一 fd 表。
pub(crate) enum OpenFileKind {
    /// 经字符设备注册表打开、由设备子系统实现的设备文件。
    Device(Arc<dyn crate::fs::device::DeviceFile>),
    Pipe(Arc<PipeEnd>),
    Socket(Arc<Socket>),
    Epoll(Arc<Epoll>),
    EventFd(Arc<EventFd>),
    TimerFd(Arc<TimerFd>),
    Inode(Arc<OpenedFile>),
    MemFile(Arc<crate::fs::MemFile>),
}

/// console 文件后端 seam；具体 platform adapter 只在 composition root 装配。
pub(crate) trait Console: Send + Sync {
    /// 非阻塞读取当前 IRQ ring 中已有 console bytes。
    ///
    /// # Parameters
    ///
    /// - `bytes`: kernel-owned 输出缓冲区。
    ///
    /// # Returns
    ///
    /// 已有输入长度；零表示调用方必须进入 console wait；设备失败返回 `IoError`。
    fn read(&self, bytes: &mut [u8]) -> Result<usize, FileSystemError>;

    /// 查询 console 是否可读，只允许在 wait owner lock 内封闭 read/enqueue race。
    fn input_ready(&self) -> bool;

    /// 原子丢弃 adapter 尚未交给 line discipline 的全部 raw input。
    ///
    /// # Returns
    ///
    /// 被丢弃的 byte 数。
    fn discard_input(&self) -> usize;

    /// 原子丢弃 adapter 尚未被终端 peer 消费的全部 output。
    ///
    /// # Returns
    ///
    /// 被丢弃的 byte 数；同步直写设备没有 pending output 时返回零。
    fn discard_output(&self) -> usize;

    /// 同步且不睡眠等待地写出完整或部分 console 字节流。
    ///
    /// # Parameters
    ///
    /// - `bytes`: kernel 已完成 user-copy 的连续字节。
    ///
    /// # Returns
    ///
    /// 实际写出长度；底层 console 失败返回 `IoError`。返回时不得保留待发送队列。
    fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError>;
}

/// Linux open file description，共享偏移和状态标志。
pub(crate) struct OpenFileDescription {
    pub(crate) kind: OpenFileKind,
    position: FilePosition,
    pub(crate) flags: Mutex<u32>,
    character_opened: Option<Arc<OpenedFile>>,
    pub(super) epoll_memberships: EpollMemberships,
    // fork 后各 fd table 使用独立锁，单表扫描无法识别最后一个 descriptor；该计数负责跨表触发
    // epoll 的 Linux close cleanup，缺失时会留下 fd reuse 可命中的旧 interest。
    descriptor_refs: AtomicUsize,
    // 以写方式打开 inode 时登记的挂载写者；Drop 时归还。缺失则 `remount,ro` 看不到仍在写的 fd。
    _write_open: Option<WriteOpen>,
}

impl Drop for OpenFileDescription {
    /// 最后一个引用消失即“关闭”：向 inotify 投递 `IN_CLOSE_WRITE`/`IN_CLOSE_NOWRITE`（Linux `fput`）。
    fn drop(&mut self) {
        if let OpenFileKind::Inode(opened) = &self.kind
            && crate::fs::watching()
        {
            let writable = *self.flags.get_mut() & O_ACCMODE != O_RDONLY;
            crate::fs::notify_opened(
                opened,
                if writable {
                    crate::fs::IN_CLOSE_WRITE
                } else {
                    crate::fs::IN_CLOSE_NOWRITE
                },
            );
        }
    }
}

/// 以写方式打开 inode 时的登记，Drop 时撤销：挂载上的写者（Linux `mnt_drop_write` 的 OFD 版本），
/// 块设备节点还登记 bdev 写者（`BlockNode`），使“被挂载”与“有写者”互斥。
struct WriteOpen {
    filesystem: usize,
    block: Option<Arc<BlockNode>>,
}

impl Drop for WriteOpen {
    fn drop(&mut self) {
        crate::fs::vfs().end_write_open(self.filesystem);
        if let Some(block) = &self.block {
            block.end_writer();
        }
    }
}

impl OpenFileDescription {
    fn socket_poll_events(events: i16, state: crate::socket::SocketPollState) -> i16 {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        const READ_HANGUP: i16 = 0x2000;
        let mut result = 0;
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
        result
    }

    /// 在该 OFD 共享 position 的唯一临界区内执行一次完整操作。
    ///
    /// # Parameters
    ///
    /// - `operation`: 依赖并可推进当前 position 的完整 operation。
    ///
    /// # Returns
    ///
    /// operation 的原始返回值。
    pub(crate) fn with_position<R>(&self, operation: impl FnOnce(&mut u64) -> R) -> R {
        self.position.with(operation)
    }

    /// 返回该 OFD 共享 position 的瞬时快照，不推进 position。
    ///
    /// # Returns
    ///
    /// 当前共享 position。
    pub(crate) fn position_snapshot(&self) -> u64 {
        self.position.snapshot()
    }

    /// 原子计算并发布 signed Linux file position；失败时保持原值。
    ///
    /// # Parameters
    ///
    /// - `offset`: signed byte delta。
    /// - `base`: 把当前 position 投影为本次 seek 基准的 closure。
    ///
    /// # Returns
    ///
    /// 成功发布的新 position。
    ///
    /// # Errors
    ///
    /// 结果为负或超出 `i64::MAX` 时返回错误。
    pub(crate) fn seek_position(
        &self,
        offset: i64,
        base: impl FnOnce(u64) -> u64,
    ) -> Result<u64, ()> {
        self.position.seek(offset, base)
    }

    /// 按全局地址顺序锁定两个不同 OFD 的 positions，并保持 caller 参数顺序。
    ///
    /// 同一 OFD 返回 `None`，caller 必须单独定义单 position 的操作语义。
    ///
    /// # Parameters
    ///
    /// - `first`: caller 语义中的第一个 OFD。
    /// - `second`: caller 语义中的第二个 OFD。
    /// - `operation`: 同时依赖并可推进两个 positions 的完整 operation。
    ///
    /// # Returns
    ///
    /// OFD 不同时返回 operation 结果；相同时返回 `None`。
    pub(crate) fn with_positions<R>(
        first: &Self,
        second: &Self,
        operation: impl FnOnce(&mut u64, &mut u64) -> R,
    ) -> Option<R> {
        FilePosition::with_pair(&first.position, &second.position, operation)
    }

    /// 从唯一 OFD backend 投影 poll/epoll readiness，不注册 waiter。
    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let mut result = 0;
        match &self.kind {
            OpenFileKind::Inode(_) | OpenFileKind::MemFile(_) => result = events & (INPUT | OUTPUT),
            OpenFileKind::Device(file) => result = file.poll(events),
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
                result = Self::socket_poll_events(events, socket.poll_state());
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
            OpenFileKind::TimerFd(timer) => {
                if events & INPUT != 0 && timer.readable() {
                    result |= INPUT;
                }
            }
        }
        result
    }

    /// 在 deferred source 通知中无等待地投影 OFD readiness。
    ///
    /// # Parameters
    ///
    /// - `events`: caller 关注的 poll event mask。
    ///
    /// # Returns
    ///
    /// backend 可立即观察时返回 event bits；owner 竞争时返回 `None`。
    ///
    /// # Errors
    ///
    /// 不分配、不睡眠，也不注册 task waiter。
    pub(crate) fn try_poll_events(&self, events: i16) -> Option<i16> {
        match &self.kind {
            OpenFileKind::Socket(socket) => socket
                .try_poll_state()
                .map(|state| Self::socket_poll_events(events, state)),
            _ => Some(self.poll_events(events)),
        }
    }

    /// 返回当前 OFD 最近一次可观察 I/O 状态变化的全局 generation。
    ///
    /// # Parameters
    ///
    /// - `events`: caller 关注的 poll event mask。
    ///
    /// # Returns
    ///
    /// 跨 source 可比较的 generation；不支持 epoll 的 inode/device 返回零。
    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        match &self.kind {
            OpenFileKind::Device(file) => file.readiness_generation(),
            OpenFileKind::Pipe(endpoint) => {
                endpoint.pipe().readiness_generation(endpoint.direction())
            }
            OpenFileKind::Socket(socket) => socket.readiness_generation(events),
            OpenFileKind::Epoll(epoll) => epoll.readiness_generation(),
            OpenFileKind::EventFd(event) => event.readiness_generation(events),
            OpenFileKind::TimerFd(timer) => timer.readiness_generation(),
            OpenFileKind::Inode(_) | OpenFileKind::MemFile(_) => 0,
        }
    }

    /// 判断 backend 是否提供可注册 wait source，而非仅提供同步 poll 结果。
    ///
    /// # Returns
    ///
    /// 可加入 epoll 返回 true；regular inode/null/zero 返回 false 并映射 EPERM。
    pub(crate) fn epoll_pollable(&self) -> bool {
        match &self.kind {
            OpenFileKind::Device(file) => !file.wait_sources(0x001 | 0x004).is_empty(),
            OpenFileKind::Pipe(_)
            | OpenFileKind::Socket(_)
            | OpenFileKind::Epoll(_)
            | OpenFileKind::EventFd(_)
            | OpenFileKind::TimerFd(_) => true,
            OpenFileKind::Inode(_) | OpenFileKind::MemFile(_) => false,
        }
    }

    /// 把 OFD 投影为 epoll 持久 source index 使用的固定 source 集合。
    ///
    /// # Parameters
    ///
    /// - `events`: interest event mask；决定是否需要 read/write 两个方向。
    ///
    /// # Returns
    ///
    /// 最多两个稳定 source identity；无异步 source 返回空集合。
    pub(crate) fn readiness_sources(&self, events: i16) -> ReadinessSources {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        let mut sources = ReadinessSources::new();
        match &self.kind {
            OpenFileKind::Device(file) => {
                for source in file.wait_sources(events).iter() {
                    sources.push(match source {
                        crate::fs::device::DeviceWaitSource::Pipe {
                            pipe, direction, ..
                        } => ReadinessSource::pipe(pipe, *direction),
                        crate::fs::device::DeviceWaitSource::Console => ReadinessSource::Console,
                    });
                }
            }
            OpenFileKind::Pipe(endpoint) => sources.push(ReadinessSource::pipe(
                &endpoint.pipe(),
                endpoint.direction(),
            )),
            OpenFileKind::Socket(socket) => {
                let (socket_sources, _) = socket.wait_sources(events);
                for source in socket_sources.into_iter().flatten() {
                    match source {
                        crate::socket::SocketWaitSource::Notification(pipe) => sources.push(
                            ReadinessSource::pipe(&pipe, crate::ipc::PipeDirection::Read),
                        ),
                        crate::socket::SocketWaitSource::Data { pipe, direction } => {
                            sources.push(ReadinessSource::pipe(&pipe, direction));
                        }
                    }
                }
            }
            OpenFileKind::Epoll(epoll) => sources.push(ReadinessSource::pipe(
                &epoll.notification_pipe(),
                crate::ipc::PipeDirection::Read,
            )),
            OpenFileKind::EventFd(event) => {
                if events & INPUT != 0 {
                    sources.push(ReadinessSource::pipe(
                        &event.notification_pipe(true),
                        crate::ipc::PipeDirection::Read,
                    ));
                }
                if events & OUTPUT != 0 {
                    sources.push(ReadinessSource::pipe(
                        &event.notification_pipe(false),
                        crate::ipc::PipeDirection::Read,
                    ));
                }
            }
            OpenFileKind::TimerFd(timer) if events & INPUT != 0 => {
                sources.push(ReadinessSource::pipe(
                    &timer.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                ));
            }
            _ => {}
        }
        sources
    }

    /// 构造经字符设备注册表打开的设备 OFD。
    ///
    /// # Parameters
    ///
    /// - `file`: driver 返回的打开设备文件。
    /// - `flags`: OFD status flags。
    /// - `backing_opened`: 打开时的 devfs opened entry，用于 metadata、fstatfs 与 procfs。
    ///
    /// # Errors
    ///
    /// OFD 分配失败返回 `OutOfMemory`。
    pub(crate) fn device(
        file: Arc<dyn crate::fs::device::DeviceFile>,
        flags: u32,
        backing_opened: Arc<OpenedFile>,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            kind: OpenFileKind::Device(file),
            position: FilePosition::new(),
            flags: Mutex::new(flags),
            character_opened: Some(backing_opened),
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 构造没有路径的设备 OFD（inotify 等 anonymous inode）。
    ///
    /// # Errors
    ///
    /// OFD 分配失败返回 `OutOfMemory`。
    pub(crate) fn anonymous_device(
        file: Arc<dyn crate::fs::device::DeviceFile>,
        flags: u32,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            kind: OpenFileKind::Device(file),
            position: FilePosition::new(),
            flags: Mutex::new(flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 构造 pathname-backed regular/directory OFD。
    ///
    /// 以写方式打开（`O_WRONLY`/`O_RDWR`）时在挂载上登记一个写者，OFD 释放时撤销。
    ///
    /// # Errors
    ///
    /// 挂载为 read-only 返回 `ReadOnly`；分配失败返回 `OutOfMemory`。
    pub(crate) fn inode(opened: Arc<OpenedFile>, flags: u32) -> Result<Arc<Self>, FileSystemError> {
        let write_open = if flags & O_ACCMODE != O_RDONLY {
            let inode = opened.inode();
            let filesystem = inode.filesystem_id();
            crate::fs::vfs().begin_write_open(filesystem)?;
            // 先构造 guard：后面的 bdev 登记失败时，它的 Drop 撤销已完成的挂载写者登记。
            let mut guard = WriteOpen {
                filesystem,
                block: None,
            };
            if inode.inode_type() == InodeType::BlockDevice {
                let block = inode
                    .device_number()
                    .and_then(crate::fs::device::block_node)
                    .ok_or(FileSystemError::NoDevice)?;
                block.begin_writer()?;
                guard.block = Some(block);
            }
            Some(guard)
        } else {
            None
        };
        Arc::try_new(Self {
            kind: OpenFileKind::Inode(opened),
            position: FilePosition::new(),
            flags: Mutex::new(flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: write_open,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 构造 pathname-less memfd OFD。
    pub(crate) fn mem_file(file: Arc<crate::fs::MemFile>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::MemFile(file),
            position: FilePosition::new(),
            flags: Mutex::new(flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| ())
    }

    pub(crate) fn pipe(endpoint: Arc<PipeEnd>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::Pipe(endpoint),
            position: FilePosition::new(),
            flags: Mutex::new(flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| ())
    }

    pub(crate) fn socket(socket: Arc<Socket>, flags: u32) -> Result<Arc<Self>, ()> {
        let ofd = Arc::try_new(Self {
            kind: OpenFileKind::Socket(socket.clone()),
            position: FilePosition::new(),
            flags: Mutex::new(flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| ())?;
        let owner: Arc<dyn UnixPassedFile> = ofd.clone();
        socket.bind_unix_rights_owner(Arc::downgrade(&owner));
        Ok(ofd)
    }

    pub(crate) fn epoll(epoll: Arc<Epoll>) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::Epoll(epoll),
            position: FilePosition::new(),
            flags: Mutex::new(O_RDWR),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| ())
    }

    pub(crate) fn event_fd(event: Arc<EventFd>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::EventFd(event),
            position: FilePosition::new(),
            flags: Mutex::new(O_RDWR | flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| ())
    }

    pub(crate) fn timer_fd(timer: Arc<TimerFd>, flags: u32) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            kind: OpenFileKind::TimerFd(timer),
            position: FilePosition::new(),
            flags: Mutex::new(O_RDWR | flags),
            character_opened: None,
            epoll_memberships: EpollMemberships::new(),
            descriptor_refs: AtomicUsize::new(0),
            _write_open: None,
        })
        .map_err(|_| ())
    }

    pub(crate) fn inode_ref(&self) -> Option<Arc<dyn Inode>> {
        match &self.kind {
            OpenFileKind::Inode(opened) => Some(opened.inode()),
            OpenFileKind::MemFile(file) => Some(file.clone()),
            OpenFileKind::Device(_) => None,
            OpenFileKind::Pipe(_)
            | OpenFileKind::Socket(_)
            | OpenFileKind::Epoll(_)
            | OpenFileKind::EventFd(_)
            | OpenFileKind::TimerFd(_) => None,
        }
    }

    /// 返回 pathname-backed OFD 的稳定 opened-entry identity。
    ///
    /// # Returns
    ///
    /// regular/directory/character OFD 返回 opened entry；anonymous OFD 返回 None。
    pub(crate) fn opened_ref(&self) -> Option<Arc<OpenedFile>> {
        match &self.kind {
            OpenFileKind::Inode(opened) => Some(opened.clone()),
            OpenFileKind::MemFile(_) => None,
            OpenFileKind::Device(_) => self.character_opened.clone(),
            OpenFileKind::Pipe(_)
            | OpenFileKind::Socket(_)
            | OpenFileKind::Epoll(_)
            | OpenFileKind::EventFd(_)
            | OpenFileKind::TimerFd(_) => None,
        }
    }

    /// 取得该 OFD backing filesystem 的统计；anonymous pipe 使用 pipefs 语义。
    ///
    /// # Returns
    ///
    /// mounted inode 的 VFS 快照，或 Linux simple_statfs 形状的 pipefs 快照。
    ///
    /// # Errors
    ///
    /// 无 backing filesystem 的 OFD 返回 `InvalidFileSystem`。
    pub(crate) fn filesystem_statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        match &self.kind {
            OpenFileKind::Inode(opened) => vfs().statistics(opened.inode()),
            OpenFileKind::MemFile(_) => Ok(FileSystemStatistics {
                type_name: "tmpfs",
                magic: 0x0102_1994,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                files: 1,
                files_free: 0,
                fsid: [0x4d45_4d46, 0],
                name_length: 255,
                fragment_size: 4096,
                flags: 0x20,
            }),
            OpenFileKind::Device(_) => vfs().statistics(
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
            OpenFileKind::Epoll(_) | OpenFileKind::EventFd(_) | OpenFileKind::TimerFd(_) => {
                Err(FileSystemError::InvalidFileSystem)
            }
        }
    }
}
