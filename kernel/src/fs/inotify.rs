//! inotify：文件变更通知（Linux `fs/notify/inotify`）。
//!
//! 一个 inotify fd 是一个 [`Inotify`] 实例：它拥有自己的 watch 列表和有界事件队列，经 [`DeviceFile`]
//! 接入 OFD，所以阻塞 read、poll/epoll、`FIONREAD` 都走设备 seam。内核里发生变更的地方调用本模块的
//! `notify_*`：它们先看一个全局计数，没有任何 watch 时是一次原子读取就返回，对 read/write 热路径
//! 没有可测量的成本。
//!
//! 并发：全局 [`REGISTRY`] 按 inode 身份保存 watch，投递时只在锁内克隆出匹配的 `Arc<Watch>`，随后
//! 在锁外逐个入队并唤醒 reader，所以不会在持有注册表锁时取队列锁或调度器锁。锁顺序：实例的 watch
//! 列表 → 注册表；任何路径都不在持有注册表锁时再取实例的 watch 列表。

use alloc::{sync::Arc, sync::Weak, vec::Vec};
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;
use syscall_abi::errno;

use super::{
    Inode, InodeType, OpenedFile,
    device::{self, DeviceError, DeviceFile, DeviceWaitSources, IoctlCall, UserFault, UserOutput},
};
use crate::{
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeEnd},
    sync::TaskMutex,
};
use queue::{Event, EventQueue};

#[path = "inotify/queue.rs"]
mod queue;

pub(crate) const IN_ACCESS: u32 = 0x0000_0001;
pub(crate) const IN_MODIFY: u32 = 0x0000_0002;
pub(crate) const IN_ATTRIB: u32 = 0x0000_0004;
pub(crate) const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub(crate) const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub(crate) const IN_OPEN: u32 = 0x0000_0020;
pub(crate) const IN_MOVED_FROM: u32 = 0x0000_0040;
pub(crate) const IN_MOVED_TO: u32 = 0x0000_0080;
pub(crate) const IN_CREATE: u32 = 0x0000_0100;
pub(crate) const IN_DELETE: u32 = 0x0000_0200;
pub(crate) const IN_DELETE_SELF: u32 = 0x0000_0400;
pub(crate) const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_UNMOUNT: u32 = 0x0000_2000;
const IN_IGNORED: u32 = 0x0000_8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
/// 可订阅的事件位。
const ALL_EVENTS: u32 = IN_ACCESS
    | IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_CLOSE_NOWRITE
    | IN_OPEN
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF;
const ADD_WATCH_FLAGS: u32 = IN_ONLYDIR | IN_DONT_FOLLOW | IN_MASK_ADD | IN_ONESHOT;

/// `inotify_init1` 接受的 flags：`O_NONBLOCK` 与 `O_CLOEXEC`。
pub(crate) const IN_NONBLOCK: u32 = 0o4000;
pub(crate) const IN_CLOEXEC: u32 = 0o2000000;

/// 同时存在的 inotify 实例上限（Linux `max_user_instances` 缺省）；没有用户资源模型，全局计。
const MAX_INSTANCES: usize = 128;
/// 单个实例的 watch 上限；超过返回 `ENOSPC`（Linux `max_user_watches`）。
const MAX_WATCHES: usize = 8192;
/// 单次 read 编码的字节上限；更大的用户缓冲分多次读取。
const READ_BATCH: usize = 64 * 1024;
const POLLIN: i16 = 0x001;

/// 一个 inode 上的一个 watch。
struct Watch {
    identity: (usize, u64),
    instance: Weak<Inotify>,
    wd: i32,
    mask: AtomicU32,
}

/// inode 身份 → 其上全部 watch 的唯一索引，只在 watch 创建/删除时修改；缺失时每个变更点都得扫描所有
/// 实例的 watch 列表。
struct WatchIndex {
    by_inode: FallibleMap<(usize, u64), Vec<Arc<Watch>>>,
}

// OWNER: 全局 watch 索引的唯一实例。
static REGISTRY: Mutex<WatchIndex> = Mutex::new(WatchIndex {
    by_inode: FallibleMap::new(),
});
// OWNER: 全局 watch 总数。它是所有 `notify_*` 的快速路径：为零时变更点只付出一次 Relaxed 读取。
static WATCH_COUNT: AtomicUsize = AtomicUsize::new(0);
// OWNER: 存活的 inotify 实例数，执行 `MAX_INSTANCES`；在 `Inotify::new` 预留、Drop 归还。
static INSTANCES: AtomicUsize = AtomicUsize::new(0);

/// `add_watch` 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchError {
    /// 事件位为空或含未知位（`EINVAL`）。
    InvalidMask,
    /// 该实例的 watch 数已达上限（`ENOSPC`）。
    TooManyWatches,
    OutOfMemory,
}

/// 创建实例的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitError {
    /// 实例数已达上限（`EMFILE`）。
    TooManyInstances,
    OutOfMemory,
}

/// 一个 inotify 实例（一个 inotify fd 背后的对象）。
pub(crate) struct Inotify {
    queue: Mutex<EventQueue>,
    watches: Mutex<Vec<Arc<Watch>>>,
    next_wd: AtomicI32,
    // 合并的读就绪 edge：入队后 signal，阻塞的 reader 与 poll/epoll 经它唤醒。
    notification: (Arc<PipeEnd>, Arc<PipeEnd>),
    // OWNER: 序列化同一个 OFD（dup/fork 共享）的并发 reader。编码后的字节要写到用户内存并可能缺页，
    // 事件在交付成功之后才出队；缺失时两个 reader 会各拿到同一批事件。
    read_gate: TaskMutex<()>,
    // 实例自己的弱引用，供 watch 回指；`DeviceFile` 方法只拿到 `&self`，没有 `Arc`。构造后立即设置一次。
    this: Mutex<Weak<Inotify>>,
}

impl Inotify {
    /// 创建一个空实例。
    pub(crate) fn new() -> Result<Arc<Self>, InitError> {
        INSTANCES
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                (count < MAX_INSTANCES).then(|| count + 1)
            })
            .map_err(|_| InitError::TooManyInstances)?;
        // 此后任何失败都要归还实例名额；成功构造后由 Drop 归还。
        let release = |error| {
            INSTANCES.fetch_sub(1, Ordering::AcqRel);
            error
        };
        let notification =
            Pipe::notification_pair().map_err(|()| release(InitError::OutOfMemory))?;
        let instance = Arc::try_new(Self {
            queue: Mutex::new(EventQueue::new()),
            watches: Mutex::new(Vec::new()),
            next_wd: AtomicI32::new(1),
            notification,
            read_gate: TaskMutex::new(()),
            this: Mutex::new(Weak::new()),
        })
        .map_err(|_| release(InitError::OutOfMemory))?;
        *instance.this.lock() = Arc::downgrade(&instance);
        Ok(instance)
    }

    /// 入队一个事件并唤醒 reader。
    fn enqueue(&self, wd: i32, mask: u32, cookie: u32, name: &[u8]) {
        let mut owned = Vec::new();
        let queued = if owned.try_reserve_exact(name.len()).is_ok() {
            owned.extend_from_slice(name);
            self.queue.lock().push(Event {
                wd,
                mask,
                cookie,
                name: owned.into_boxed_slice(),
            })
        } else {
            // 名字分配失败按溢出处理：用户至少会收到“丢了事件”的信号。
            self.queue.lock().push(Event {
                wd: -1,
                mask: queue::IN_Q_OVERFLOW,
                cookie: 0,
                name: Default::default(),
            })
        };
        // 队列锁已释放；合并掉的事件没有新的可读内容，不需要唤醒。
        if queued != queue::Pushed::Coalesced {
            self.notification.1.signal_readiness();
        }
    }

    /// 在 `identity` 上添加或更新 watch。
    ///
    /// 同一实例对同一 inode 的 watch 只有一个：重复添加更新掩码（`IN_MASK_ADD` 为合并）并返回原 wd。
    ///
    /// # Errors
    ///
    /// 见 [`WatchError`]。
    pub(crate) fn add_watch(&self, identity: (usize, u64), flags: u32) -> Result<i32, WatchError> {
        let events = flags & (ALL_EVENTS | IN_ONESHOT);
        if flags & ALL_EVENTS == 0 || flags & !(ALL_EVENTS | ADD_WATCH_FLAGS) != 0 {
            return Err(WatchError::InvalidMask);
        }
        let mut watches = self.watches.lock();
        if let Some(existing) = watches.iter().find(|watch| watch.identity == identity) {
            let merged = if flags & IN_MASK_ADD != 0 {
                existing.mask.load(Ordering::Relaxed) | events
            } else {
                events
            };
            existing.mask.store(merged, Ordering::Relaxed);
            return Ok(existing.wd);
        }
        if watches.len() >= MAX_WATCHES {
            return Err(WatchError::TooManyWatches);
        }
        watches
            .try_reserve(1)
            .map_err(|_| WatchError::OutOfMemory)?;
        let watch = Arc::try_new(Watch {
            identity,
            instance: self.this.lock().clone(),
            wd: self.next_wd.fetch_add(1, Ordering::Relaxed),
            mask: AtomicU32::new(events),
        })
        .map_err(|_| WatchError::OutOfMemory)?;
        {
            let mut registry = REGISTRY.lock();
            match registry.by_inode.get_mut(&identity) {
                Some(list) => {
                    list.try_reserve(1).map_err(|_| WatchError::OutOfMemory)?;
                    list.push(watch.clone());
                }
                None => {
                    let mut list = Vec::new();
                    list.try_reserve_exact(1)
                        .map_err(|_| WatchError::OutOfMemory)?;
                    list.push(watch.clone());
                    registry
                        .by_inode
                        .try_insert(identity, list)
                        .map_err(|_| WatchError::OutOfMemory)?;
                }
            }
        }
        let wd = watch.wd;
        watches.push(watch);
        WATCH_COUNT.fetch_add(1, Ordering::AcqRel);
        Ok(wd)
    }

    /// 删除 `wd` 并投递 `IN_IGNORED`（`inotify_rm_watch`）。
    ///
    /// # Returns
    ///
    /// wd 不属于本实例返回 `false`。
    pub(crate) fn remove_watch(&self, wd: i32) -> bool {
        let watch = {
            let mut watches = self.watches.lock();
            match watches.iter().position(|watch| watch.wd == wd) {
                Some(index) => watches.swap_remove(index),
                None => return false,
            }
        };
        unregister(&watch);
        self.enqueue(wd, IN_IGNORED, 0, b"");
        true
    }

    /// 内核自己撤销 watch（inode 已删除、文件系统卸载、`IN_ONESHOT`）：与 [`Self::remove_watch`] 相同，
    /// 但先按需投递 `extra`。
    fn revoke(&self, wd: i32, extra: u32) {
        if extra != 0 {
            self.enqueue(wd, extra, 0, b"");
        }
        self.remove_watch(wd);
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        for watch in self.watches.get_mut().drain(..) {
            unregister(&watch);
        }
        INSTANCES.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 把 `watch` 从全局注册表移除。
fn unregister(watch: &Arc<Watch>) {
    let mut registry = REGISTRY.lock();
    if let Some(list) = registry.by_inode.get_mut(&watch.identity) {
        list.retain(|candidate| !Arc::ptr_eq(candidate, watch));
        if list.is_empty() {
            registry.by_inode.remove(&watch.identity);
        }
    }
    drop(registry);
    WATCH_COUNT.fetch_sub(1, Ordering::AcqRel);
}

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

impl DeviceFile for Inotify {
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let _gate = self
            .read_gate
            .lock()
            .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
        loop {
            let encoded = self
                .queue
                .lock()
                .encode_prefix(output.remaining().min(READ_BATCH));
            match encoded {
                // 第一个事件就放不下用户缓冲（Linux：`EINVAL`）。
                Err(()) => return Err(DeviceError::Errno(errno::EINVAL)),
                Ok((_, 0)) => device::wait_ready(self, POLLIN, nonblocking)?,
                Ok((bytes, count)) => {
                    output.write(&bytes).map_err(fault)?;
                    // 交付成功才出队：写用户内存失败的事件留在队列里。
                    self.queue.lock().discard(count);
                    return Ok(());
                }
            }
        }
    }

    fn poll(&self, events: i16) -> i16 {
        if !self.queue.lock().is_empty() {
            events & POLLIN
        } else {
            0
        }
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        if events & POLLIN != 0 {
            DeviceWaitSources::pipe(self.notification.0.pipe(), POLLIN)
        } else {
            DeviceWaitSources::new()
        }
    }

    fn readiness_generation(&self) -> u64 {
        self.notification
            .0
            .pipe()
            .readiness_generation(crate::ipc::PipeDirection::Read)
    }

    /// 阻塞前排空已消费的合并 edge 再复查，避免排空与新事件之间丢唤醒。
    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll(events) != 0 {
            return None;
        }
        self.notification.0.drain_readiness();
        (self.poll(events) == 0).then(|| self.notification.0.pipe())
    }

    /// `FIONREAD`：排队事件的线格式总字节数。
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        const FIONREAD: usize = 0x541b;
        if call.request != FIONREAD {
            return Err(DeviceError::Errno(errno::ENOTTY));
        }
        let pending = i32::try_from(self.queue.lock().pending_bytes()).unwrap_or(i32::MAX);
        call.user
            .write(call.argument, &pending.to_ne_bytes())
            .map_err(fault)?;
        Ok(0)
    }

    fn inotify(&self) -> Option<&Inotify> {
        Some(self)
    }
}

// ---------------------------------------------------------------- 变更点 API

/// 是否存在任何 watch；变更点的快速路径。
#[inline]
pub(crate) fn watching() -> bool {
    WATCH_COUNT.load(Ordering::Relaxed) != 0
}

fn identity_of(inode: &dyn Inode) -> Option<(usize, u64)> {
    Some((inode.filesystem_id(), inode.metadata().ok()?.inode))
}

/// 把事件投递给 `identity` 上订阅了 `mask` 的全部 watch。
fn deliver(identity: (usize, u64), mask: u32, cookie: u32, name: &[u8]) {
    let targets = {
        let registry = REGISTRY.lock();
        let Some(list) = registry.by_inode.get(&identity) else {
            return;
        };
        let mut targets = Vec::new();
        if targets.try_reserve_exact(list.len()).is_err() {
            return;
        }
        targets.extend(list.iter().cloned());
        targets
    };
    for watch in targets {
        let subscribed = watch.mask.load(Ordering::Relaxed);
        if subscribed & mask & ALL_EVENTS == 0 {
            continue;
        }
        let Some(instance) = watch.instance.upgrade() else {
            continue;
        };
        instance.enqueue(watch.wd, mask & (ALL_EVENTS | IN_ISDIR), cookie, name);
        if subscribed & IN_ONESHOT != 0 {
            instance.revoke(watch.wd, 0);
        }
    }
}

/// `opened` 指向的文件发生了 `mask`：投递给文件自身的 watch（无名字），再投递给父目录的 watch（带
/// 文件名）。用于 open/close/read/write/属性变化。
pub(crate) fn notify_opened(opened: &Arc<OpenedFile>, mask: u32) {
    if !watching() {
        return;
    }
    let inode = opened.inode();
    let Some(identity) = identity_of(inode.as_ref()) else {
        return;
    };
    let mask = if inode.inode_type() == InodeType::Directory {
        mask | IN_ISDIR
    } else {
        mask
    };
    deliver(identity, mask, 0, b"");
    if let Some((parent, name)) = opened.watch_context()
        && let Some(parent_identity) = identity_of(parent.as_ref())
    {
        deliver(parent_identity, mask, 0, &name);
    }
}

/// 目录 `parent` 里的名字 `name` 发生了 `mask`（create/delete/moved）；只投递给父目录的 watch。
pub(crate) fn notify_entry(
    parent: &dyn Inode,
    name: &[u8],
    mask: u32,
    is_directory: bool,
    cookie: u32,
) {
    if !watching() {
        return;
    }
    if let Some(identity) = identity_of(parent) {
        let mask = if is_directory { mask | IN_ISDIR } else { mask };
        deliver(identity, mask, cookie, name);
    }
}

/// inode 自身发生了 `mask`（`IN_ATTRIB` 在 link count 变化、`IN_MOVE_SELF`），没有名字。
pub(crate) fn notify_self(inode: &dyn Inode, mask: u32) {
    if !watching() {
        return;
    }
    if let Some(identity) = identity_of(inode) {
        let mask = if inode.inode_type() == InodeType::Directory {
            mask | IN_ISDIR
        } else {
            mask
        };
        deliver(identity, mask, 0, b"");
    }
}

/// inode 身份（unlink/rename 之前取得：之后 inode 可能已被回收，metadata 不再可靠）。
///
/// 没有任何 watch 时返回 `None`，调用者的 inotify 工作随之全部跳过。
pub(crate) fn identity(inode: &dyn Inode) -> Option<(usize, u64)> {
    if watching() { identity_of(inode) } else { None }
}

/// inode 的最后一个 link 已删除：投递 `IN_DELETE_SELF` 并撤销其上全部 watch（各得 `IN_IGNORED`）。
///
/// 与 Linux 的差别：Linux 在 inode 真正被回收（最后一个打开引用消失）时才投递 `IN_DELETE_SELF`；
/// 这里在最后一个 link 消失时投递。
pub(crate) fn notify_removed(identity: (usize, u64), is_directory: bool) {
    if !watching() {
        return;
    }
    deliver(
        identity,
        IN_DELETE_SELF | if is_directory { IN_ISDIR } else { 0 },
        0,
        b"",
    );
    revoke_all(|candidate| candidate == identity, 0);
}

/// 为一对 `IN_MOVED_FROM`/`IN_MOVED_TO` 分配相同的 cookie。
pub(crate) fn next_cookie() -> u32 {
    // OWNER: rename 事件配对 cookie；只需要在相邻事件间唯一，回绕无害。
    static COOKIE: AtomicU32 = AtomicU32::new(1);
    COOKIE.fetch_add(1, Ordering::Relaxed)
}

/// 文件系统被卸载：其上全部 watch 得到 `IN_UNMOUNT` 与 `IN_IGNORED`。
pub(crate) fn filesystem_unmounted(filesystem_id: usize) {
    if watching() {
        revoke_all(|identity| identity.0 == filesystem_id, IN_UNMOUNT);
    }
}

/// 撤销身份满足 `matches` 的全部 watch，先对每个投递 `extra`（非零时）。
fn revoke_all(matches: impl Fn((usize, u64)) -> bool, extra: u32) {
    let doomed = {
        let registry = REGISTRY.lock();
        let mut doomed = Vec::new();
        for (identity, list) in registry.by_inode.iter() {
            if matches(*identity) {
                if doomed.try_reserve(list.len()).is_err() {
                    break;
                }
                doomed.extend(list.iter().cloned());
            }
        }
        doomed
    };
    for watch in doomed {
        if let Some(instance) = watch.instance.upgrade() {
            instance.revoke(watch.wd, extra);
        }
    }
}

/// 解析 `inotify_add_watch` 的路径选项。
pub(crate) fn add_watch_flags(mask: u32) -> (bool, bool) {
    (mask & IN_DONT_FOLLOW != 0, mask & IN_ONLYDIR != 0)
}
