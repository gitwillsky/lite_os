mod description_preparation;
mod descriptor_table;
pub(crate) use description_preparation::{PreparedDeviceDescription, PreparedFileDescription};
mod socket_preparation;
pub(crate) use socket_preparation::PreparedSocketDescription;
mod epoll;
mod position;
mod readiness;
mod signalfd;
mod timerfd;

pub(crate) use descriptor_table::{
    DetachedFileDescriptor, FileDescriptorError, FileDescriptorTable, MAX_FILE_DESCRIPTORS,
};
pub(crate) use epoll::{Epoll, EpollChange, EpollChangeError, EpollEvent, EpollMemberships};
pub(crate) use readiness::{ReadinessSource, ReadinessSources};
pub(crate) use signalfd::{FileReadinessContext, SignalFd, SignalWaitSource};
pub(crate) use timerfd::{TimerError, TimerFd, TimerFdBackend, TimerFdRead, TimerSetting};

use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;
use spin::Mutex;

use position::FilePosition;
pub(crate) use position::PositionError;

use crate::fs::{
    BlockNode, FileSystemError, FileSystemStatistics, Inode, LockHolder, O_ACCMODE, O_APPEND,
    O_NONBLOCK, O_RDONLY, O_RDWR, OpenedFile, vfs,
};
use crate::{
    ipc::{EventFd, PipeEnd},
    socket::Socket,
};

mod unix_passed;

/// OFD 后端；character device、pipe 和 inode 共享同一 fd 表。
pub(crate) enum OpenFileKind {
    /// 经字符设备注册表打开、由设备子系统实现的设备文件。
    Device(Arc<dyn crate::fs::chardev::DeviceFile>),
    Pipe(Arc<PipeEnd>),
    Socket(Arc<Socket>),
    Epoll(Arc<Epoll>),
    EventFd(Arc<EventFd>),
    TimerFd(Arc<TimerFd>),
    SignalFd(Arc<SignalFd>),
    Inode,
}

/// Linux open file description，共享偏移和状态标志。
pub(crate) struct OpenFileDescription {
    pub(crate) kind: OpenFileKind,
    position: FilePosition,
    flags: Mutex<u32>,
    opened: Arc<OpenedFile>,
    pub(super) epoll_memberships: EpollMemberships,
    // fork 后各 fd table 使用独立锁，单表扫描无法识别最后一个 descriptor；该计数负责跨表触发
    // epoll 的 Linux close cleanup，缺失时会留下 fd reuse 可命中的旧 interest。
    descriptor_refs: AtomicUsize,
    // filesystem 的 per-open 状态（FUSE 文件句柄）；字段 drop 即 release，在 Drop 发出 close 通知之后。
    // 缺失时守护进程收不到 RELEASE，服务器端句柄泄漏到卸载。
    _storage_open: Option<Arc<dyn crate::fs::StorageOpen>>,
    // 以写方式打开 inode 时登记的挂载写者；Drop 时归还。缺失则 `remount,ro` 看不到仍在写的 fd。
    _write_open: Option<WritableInodeLease>,
}

impl Drop for OpenFileDescription {
    /// 最后一个引用消失即“关闭”：向 inotify 投递 `IN_CLOSE_WRITE`/`IN_CLOSE_NOWRITE`（Linux `fput`）。
    fn drop(&mut self) {
        if *self.flags.get_mut() & crate::fs::O_PATH != 0 {
            return;
        }
        match &self.kind {
            OpenFileKind::Socket(socket) => socket.close_description(),
            OpenFileKind::Pipe(endpoint) => endpoint.close(),
            OpenFileKind::Device(device) => device.release(),
            OpenFileKind::TimerFd(timer) => timer.close(),
            _ => {}
        }
        let writable = *self.flags.get_mut() & O_ACCMODE != O_RDONLY;
        self.notify(if writable {
            crate::fs::IN_CLOSE_WRITE
        } else {
            crate::fs::IN_CLOSE_NOWRITE
        });
        // fput 的 access→dput→mnt_drop_write 顺序：提前归还 inode pin 才能让最后
        // dentry 独占逐出 unlinked cache；mount writer 仍由末字段持有到全部 inode 清理结束。
        if let Some(write) = &mut self._write_open {
            write._content.release_inode_access();
        }
    }
}

// Debug 不能反入 metadata、position 或 backend 的锁；VMA 只显示 opaque file lifetime。
impl core::fmt::Debug for OpenFileDescription {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("OpenFileDescription")
            .finish_non_exhaustive()
    }
}
impl crate::memory::MappedFileOwner for OpenFileDescription {}

/// 以写方式打开 inode 时的登记，Drop 时撤销：挂载上的写者（Linux `mnt_drop_write` 的 OFD 版本），
/// 块设备节点还登记 bdev 写者（`BlockNode`），使“被挂载”与“有写者”互斥。
struct WritableInodeLease {
    _content: crate::fs::ContentWrite,
    block: Option<Arc<BlockNode>>,
}
impl Drop for WritableInodeLease {
    fn drop(&mut self) {
        if let Some(block) = &self.block {
            block.end_writer();
        }
    }
}

impl OpenFileDescription {
    /// 一个引用本 OFD 的 fd 被关闭（Linux `filp_close` 的 `f_op->flush`）；O_PATH 没有 file operations。
    ///
    /// # Errors
    ///
    /// filesystem 的 flush 错误，由 `close(2)` 返回给调用者。
    pub(crate) fn flush(&self, owner: usize) -> Result<(), FileSystemError> {
        match &self._storage_open {
            Some(open) if !self.is_path() => open.flush(owner as u64),
            _ => Ok(()),
        }
    }

    /// 从唯一 status flags 投影原生 FMODE_PATH；不保存第二份 backend-access 状态。
    pub(crate) fn is_path(&self) -> bool {
        self.status_flags() & crate::fs::O_PATH != 0
    }
    /// fdinfo 在实际 descriptor/OFD capture 后冻结共享 position/status；不按 pathname 重开。
    /// # Errors
    /// position acquisition/formatter OOM 或 inode metadata 错误。
    pub(crate) fn file_information(
        &self,
        descriptor_flags: u32,
        view: &crate::namespace::PidNamespace,
    ) -> Result<alloc::vec::Vec<u8>, FileSystemError> {
        let position = self
            .position_snapshot()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let flags = self.status_flags()
            | if descriptor_flags & 1 != 0 {
                crate::fs::O_CLOEXEC
            } else {
                0
            };
        let backend = match &self.kind {
            OpenFileKind::Device(file) => Some(file.as_ref()),
            _ => None,
        };
        crate::fs::format_file_information(&self.opened, position, flags, backend, view)
    }
    /// 按真实 inode 投递通用 I/O/close 通知；没有 watch 时不 clone Arc、不查 metadata。
    pub(crate) fn notify(&self, event: u32) {
        if crate::fs::watching() {
            crate::fs::notify_opened(&self.opened, event);
        }
    }

    /// 实际路径 atime consequence，与成功读取的 IN_ACCESS 独立。
    pub(crate) fn touch_access(&self) {
        self.opened.touch_access();
    }
    /// 不 pin 新 Arc 的 regular read consequence；caller 已验证请求与 backend。
    pub(crate) fn read_access(&self) -> crate::fs::ReadAccess<'_> {
        self.opened.read_access()
    }

    /// 返回 OFD 共享状态标志的瞬时快照，不包含 fd-table 的 descriptor flags。
    ///
    /// # Returns
    ///
    /// 当前 open/status flags；dup/fork descriptor 读取同一 owner。
    pub(crate) fn status_flags(&self) -> u32 {
        *self.flags.lock()
    }

    /// 按当前 F_SETFL 支持范围，原子替换 O_APPEND/O_NONBLOCK，保留其他 open flags。
    ///
    /// # Parameters
    ///
    /// - `flags`: 用户提供的 status flags；范围外的 bits 保持原值。
    ///
    /// # Returns
    ///
    /// 无；更新对共享该 OFD 的 descriptor 一次性可见。
    pub(crate) fn set_status_flags(&self, flags: u32) {
        let mut current = self.flags.lock();
        let mutable = O_APPEND | O_NONBLOCK;
        *current = (*current & !mutable) | (flags & mutable);
    }

    /// 为 FIONBIO 原子设置或清除 O_NONBLOCK，保留 O_APPEND 与所有其他 flags。
    ///
    /// # Parameters
    ///
    /// - `enabled`: true 为 nonblocking，false 为 blocking。
    ///
    /// # Returns
    ///
    /// 无；在唯一 status lock 下更新，避免覆盖并发 F_SETFL 的 O_APPEND。
    pub(crate) fn set_nonblocking(&self, enabled: bool) {
        let mut flags = self.flags.lock();
        if enabled {
            *flags |= O_NONBLOCK;
        } else {
            *flags &= !O_NONBLOCK;
        }
    }

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
        }
        if events & READ_HANGUP != 0 && state.read_closed {
            result |= READ_HANGUP;
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
    pub(crate) fn with_position<R>(
        &self,
        operation: impl FnOnce(&mut u64) -> R,
    ) -> Result<R, PositionError> {
        self.position.with(operation)
    }

    /// 不等待的 [`OpenFileDescription::with_position`]：position 被占用时返回 `None`。
    pub(crate) fn try_with_position<R>(&self, operation: impl FnOnce(&mut u64) -> R) -> Option<R> {
        self.position.try_with(operation)
    }

    /// 返回该 OFD 共享 position 的瞬时快照，不推进 position。
    ///
    /// # Returns
    ///
    /// 当前共享 position。
    pub(crate) fn position_snapshot(&self) -> Result<u64, PositionError> {
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
    ) -> Result<u64, PositionError> {
        self.position.seek(offset, base)
    }

    /// 按全局地址顺序锁定两个不同 OFD 的 positions，并保持 caller 参数顺序。
    ///
    /// 同一 OFD 返回 SameOwner；acquisition OOM 在 callback/I/O 开始前返回。
    ///
    /// # Parameters
    ///
    /// - `first`: caller 语义中的第一个 OFD。
    /// - `second`: caller 语义中的第二个 OFD。
    /// - `operation`: 同时依赖并可推进两个 positions 的完整 operation。
    ///
    /// # Returns
    ///
    /// OFD 不同时返回 operation 结果；同一 owner 或等待元数据 OOM 返回 PositionError。
    pub(crate) fn with_positions<R>(
        first: &Self,
        second: &Self,
        operation: impl FnOnce(&mut u64, &mut u64) -> R,
    ) -> Result<R, PositionError> {
        FilePosition::with_pair(&first.position, &second.position, operation)
    }

    /// 从唯一 OFD backend 投影 poll/epoll readiness，不注册 waiter。
    pub(crate) fn poll_events(&self, events: i16, context: &impl FileReadinessContext) -> i16 {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let mut result = 0;
        match &self.kind {
            OpenFileKind::Inode => result = events & (INPUT | OUTPUT),
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
                if events & INPUT != 0 && epoll.has_ready(context) {
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
            OpenFileKind::SignalFd(signal) => {
                if events & INPUT != 0 && context.signal_pending(signal.mask()) {
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
            // signalfd 的 deferred callback 只标记候选；精确 readiness 属于 epoll_wait caller。
            OpenFileKind::SignalFd(_) | OpenFileKind::Epoll(_) => None,
            _ => Some(self.poll_events(events, &readiness::SourceNotificationContext)),
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
    pub(crate) fn readiness_generation(
        &self,
        events: i16,
        context: &impl FileReadinessContext,
    ) -> u64 {
        match &self.kind {
            OpenFileKind::Device(file) => file.readiness_generation(),
            OpenFileKind::Pipe(endpoint) => {
                endpoint.pipe().readiness_generation(endpoint.direction())
            }
            OpenFileKind::Socket(socket) => socket.readiness_generation(events),
            OpenFileKind::Epoll(epoll) => epoll.readiness_generation(),
            OpenFileKind::EventFd(event) => event.readiness_generation(events),
            OpenFileKind::TimerFd(timer) => timer.readiness_generation(),
            OpenFileKind::SignalFd(_) => context
                .signal_source()
                .map_or(0, |source| source.generation()),
            OpenFileKind::Inode => 0,
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
            | OpenFileKind::TimerFd(_)
            | OpenFileKind::SignalFd(_) => true,
            OpenFileKind::Inode => false,
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
    /// 最多三个稳定 source identity；无异步 source 返回空集合。
    pub(crate) fn readiness_sources(
        &self,
        events: i16,
        context: &impl FileReadinessContext,
    ) -> ReadinessSources {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        let mut sources = ReadinessSources::new();
        match &self.kind {
            OpenFileKind::Device(file) => {
                for source in file.wait_sources(events).iter() {
                    sources.push(match source {
                        crate::fs::chardev::DeviceWaitSource::Pipe {
                            pipe, direction, ..
                        } => ReadinessSource::pipe(pipe, *direction),
                        crate::fs::chardev::DeviceWaitSource::Console => ReadinessSource::Console,
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
                    sources.push(ReadinessSource::pipe(
                        &source.into_notification(),
                        crate::ipc::PipeDirection::Read,
                    ));
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
            OpenFileKind::SignalFd(_) => {
                if let Some(source) = context.signal_source() {
                    sources.push(ReadinessSource::Signal {
                        identity: source.identity(),
                    });
                }
            }
            _ => {}
        }
        sources
    }

    pub(crate) fn pipe(
        endpoint: Arc<PipeEnd>,
        flags: u32,
        opened: Arc<OpenedFile>,
    ) -> Result<Arc<Self>, ()> {
        let prepared = PreparedFileDescription::try_new().map_err(|_| ())?;
        Ok(prepared.initialize(OpenFileKind::Pipe(endpoint), opened, flags, None, None))
    }

    /// 把 OFD 投影为 VFS advisory lock（flock/fcntl）使用的持锁者视图。
    ///
    /// # Returns
    ///
    /// 所有 OFD 携带其实际 opened inode；匿名标签不改变锁身份。
    /// `identity` 是 OFD 的 pointer identity，dup/fork 出的 descriptor 共享它。
    pub(crate) fn lock_holder(self: &Arc<Self>) -> LockHolder {
        LockHolder {
            opened: Some(self.opened_ref()),
            identity: Arc::as_ptr(self) as usize,
        }
    }

    pub(crate) fn epoll(epoll: Arc<Epoll>) -> Result<Arc<Self>, ()> {
        let prepared = PreparedFileDescription::try_new().map_err(|_| ())?;
        let opened = crate::fs::anonymous_opened(b"anon_inode:[eventpoll]").map_err(|_| ())?;
        Ok(prepared.initialize(OpenFileKind::Epoll(epoll), opened, O_RDWR, None, None))
    }

    pub(crate) fn event_fd(event: Arc<EventFd>, flags: u32) -> Result<Arc<Self>, ()> {
        let prepared = PreparedFileDescription::try_new().map_err(|_| ())?;
        let opened = crate::fs::anonymous_opened(b"anon_inode:[eventfd]").map_err(|_| ())?;
        Ok(prepared.initialize(
            OpenFileKind::EventFd(event),
            opened,
            O_RDWR | flags,
            None,
            None,
        ))
    }

    pub(crate) fn timer_fd(timer: Arc<TimerFd>, flags: u32) -> Result<Arc<Self>, ()> {
        let prepared = PreparedFileDescription::try_new().map_err(|_| ())?;
        let opened = crate::fs::anonymous_opened(b"anon_inode:[timerfd]").map_err(|_| ())?;
        Ok(prepared.initialize(
            OpenFileKind::TimerFd(timer),
            opened,
            O_RDWR | flags,
            None,
            None,
        ))
    }

    /// 在 backend 发布前准备完整 signalfd OFD；没有创建者的 Task 引用。
    pub(crate) fn signal_fd(mask: u64, flags: u32) -> Result<Arc<Self>, ()> {
        let prepared = PreparedFileDescription::try_new().map_err(|_| ())?;
        let opened = crate::fs::anonymous_opened(b"anon_inode:[signalfd]").map_err(|_| ())?;
        let signal = SignalFd::try_new(mask)?;
        Ok(prepared.initialize(
            OpenFileKind::SignalFd(signal),
            opened,
            O_RDWR | flags,
            None,
            None,
        ))
    }

    /// 返回每个 OFD 唯一的 inode identity；是否支持 I/O 由 backend 决定。
    pub(crate) fn inode_ref(&self) -> Arc<dyn Inode> {
        self.opened.inode()
    }

    /// 冻结真实 opened identity，不按 anonymous diagnostic label 查找路径。
    pub(crate) fn opened_ref(&self) -> Arc<OpenedFile> {
        self.opened.clone()
    }

    /// pathname/fd statfs 共用 inode 的实际 mounted/pseudo filesystem 事实。
    pub(crate) fn filesystem_statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        vfs().statistics(&self.opened)
    }
}
