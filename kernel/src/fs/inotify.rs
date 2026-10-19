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
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;
use syscall_abi::errno;

use super::{
    Inode, InodeType, OpenedFile,
    chardev::{self, DeviceError, DeviceFile, DeviceWaitSources, IoctlCall, UserFault, UserOutput},
};
use crate::namespace::{UserNamespace, UserResource, UserResourceCharge};
use crate::{
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeEnd},
};
use queue::{Event, EventQueue};
mod instance;
use instance::WatchMembership;

type WatchKey = (u64, i32);
struct InodeWatches {
    watches: FallibleMap<WatchKey, Arc<Watch>>,
}

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
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
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
const ADD_WATCH_FLAGS: u32 =
    IN_ONLYDIR | IN_DONT_FOLLOW | IN_MASK_ADD | IN_ONESHOT | IN_MASK_CREATE | IN_EXCL_UNLINK;
// Linux ALL_INOTIFY_BITS 同时接受只包含标准 control/diagnostic bits 的订阅；忽略非事件 mask。
const ALL_INOTIFY_BITS: u32 =
    ALL_EVENTS | ADD_WATCH_FLAGS | IN_UNMOUNT | queue::IN_Q_OVERFLOW | IN_IGNORED | IN_ISDIR;

/// `inotify_init1` 接受的 flags：`O_NONBLOCK` 与 `O_CLOEXEC`。
pub(crate) const IN_NONBLOCK: u32 = 0o4000;
pub(crate) const IN_CLOEXEC: u32 = 0o2000000;

/// Linux 同一 1% addressable-RAM policy；用实际 mark 与 inode storage 成本估计，boot 显式注入。
pub(crate) fn inotify_watch_limit(total_bytes: usize) -> u32 {
    let watch_cost =
        core::mem::size_of::<Watch>() + 2 * core::mem::size_of::<super::tmpfs::TmpInode>();
    (total_bytes / 100 / watch_cost).clamp(8192, 1_048_576) as u32
}

const POLLIN: i16 = 0x001;
// Linux read readiness 同时投影 EPOLLIN 与 EPOLLRDNORM。
const READ_EVENTS: i16 = POLLIN | 0x040;

mod limits;
pub(crate) use limits::InotifyLimit;
use limits::QUEUED_EVENT_LIMIT;

/// 一个 inode 上的一个 watch。
struct Watch {
    key: WatchKey,
    identity: (usize, u64),
    instance: Weak<Inotify>,
    wd: i32,
    mask: AtomicU32,
    // OWNER: actual mark 的原创建者 ucounts；unregister 消费，observer Arc 不保留收费。
    admission: Mutex<Option<UserResourceCharge>>,
}

/// inode 身份 → 其上全部 watch 的唯一索引，只在 watch 创建/删除时修改；缺失时每个变更点都得扫描所有
/// 实例的 watch 列表。
struct WatchIndex {
    by_inode: FallibleMap<(usize, u64), InodeWatches>,
}

// OWNER: 全局 watch 索引的唯一实例。
static REGISTRY: Mutex<WatchIndex> = Mutex::new(WatchIndex {
    by_inode: FallibleMap::new(),
});
// OWNER: 全局 watch 总数。它是所有 `notify_*` 的快速路径：为零时变更点只付出一次 Relaxed 读取。
static WATCH_COUNT: AtomicUsize = AtomicUsize::new(0);
/// `add_watch` 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchError {
    /// 没有标准 inotify bit 或包含未知位（`EINVAL`）。
    InvalidMask,
    /// 实际 open membership 已退休（`EBADF`）。
    Closed,
    /// IN_MASK_CREATE 请求已有 inode watch（`EEXIST`）。
    AlreadyExists,
    /// instance 原 real UID/UserNS 或祖先 watch 配额已达上限（`ENOSPC`）。
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
    id: u64,
    // OWNER: queue 的实际 lifetime；None 拒绝由旧 Watch Arc 发来的迟到事件。
    // event node 与 codec buffer 在锁外准备，短 guard 内不进入 allocator。
    queue: Mutex<Option<EventQueue>>,
    // OWNER: 实际实例 admission 与两种 watch 查找投影；短 guard 内只有已准备的 AVL 更新。
    watches: Mutex<Option<WatchMembership>>,
    // 合并的读就绪 edge：入队后 signal，阻塞的 reader 与 poll/epoll 经它唤醒。
    notification: (Arc<PipeEnd>, Arc<PipeEnd>),
    // 实例自己的弱引用，供 watch 回指；`DeviceFile` 方法只拿到 `&self`，没有 `Arc`。构造后立即设置一次。
    this: Mutex<Weak<Inotify>>,
}

impl Inotify {
    /// 入队一个事件并唤醒 reader。
    fn enqueue(&self, wd: i32, mask: u32, cookie: u32, name: &[u8], expected: Option<&Arc<Watch>>) {
        // 固定 Linux ABI：self move/delete 从不携带 IN_ISDIR，目录 entry events 仍保留。
        let mask = if mask & (IN_MOVE_SELF | IN_DELETE_SELF) != 0 {
            mask & !IN_ISDIR
        } else {
            mask
        };
        let mut owned = Vec::new();
        let prepared = if owned.try_reserve_exact(name.len()).is_ok() {
            owned.extend_from_slice(name);
            EventQueue::prepare(Event {
                wd,
                mask,
                cookie,
                name: owned.into_boxed_slice(),
            })
            .ok()
        } else {
            None
        };
        let allocated = prepared.is_some();
        // actual mark 与 queue publish 共用 membership→queue 顺序；旧 Arc 不再生成迟到事件。
        let mut membership = expected.map(|_| self.watches.lock());
        if expected.is_some_and(|watch| {
            !membership
                .as_ref()
                .and_then(|guard| guard.as_ref())
                .is_some_and(|owner| owner.contains(watch))
        }) {
            drop(membership);
            drop(prepared);
            return;
        }
        let (queued, rejected) = {
            let mut queue = self.queue.lock();
            let Some(queue) = queue.as_mut() else {
                drop(queue);
                drop(membership);
                drop(prepared);
                return;
            };
            queue.push(prepared)
        };
        let oneshot = expected.and_then(|watch| {
            // Linux 在 event allocation OOM 的早退分支只报告 overflow，不销毁 ONESHOT mark。
            if !allocated || watch.mask.load(Ordering::Relaxed) & IN_ONESHOT == 0 {
                return None;
            }
            membership
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .take(wd, Some(watch))
        });
        drop(membership);
        drop(rejected);
        if let Some(watch) = oneshot {
            unregister(&watch);
            self.enqueue(wd, IN_IGNORED, 0, b"", None);
        }
        // 队列锁已释放；合并掉的事件没有新的可读内容，不需要唤醒。
        if queued != queue::Pushed::Coalesced {
            self.notification.1.signal_readiness();
        }
    }

    /// 内核自己撤销 watch（inode 已删除、文件系统卸载、`IN_ONESHOT`）：与 [`Self::remove_watch`] 相同，
    /// 但先按需投递 `extra`。
    fn revoke(&self, watch: &Arc<Watch>, extra: u32) {
        if extra != 0 {
            self.enqueue(watch.wd, extra, 0, b"", Some(watch));
        }
        self.remove_watch_exact(watch.wd, Some(watch));
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        self.release_instance();
    }
}

/// 把 `watch` 从全局注册表移除。
fn unregister(watch: &Arc<Watch>) {
    let (removed, retired_inode) = {
        let mut registry = REGISTRY.lock();
        let removed = registry
            .by_inode
            .get_mut(&watch.identity)
            .and_then(|list| list.watches.remove(&watch.key));
        let retired_inode = registry
            .by_inode
            .get(&watch.identity)
            .is_some_and(|list| list.watches.is_empty())
            .then(|| registry.by_inode.remove(&watch.identity));
        (removed, retired_inode)
    };
    if removed.is_some() {
        let previous = WATCH_COUNT.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(previous, 0, "inotify watch admission underflow");
    }
    // actual quota 与全部 Arc/AVL node 退休不保留全局 registry guard。
    let admission = watch.admission.lock().take();
    drop(admission);
    drop(removed);
    drop(retired_inode);
}

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

impl DeviceFile for Inotify {
    fn supports_write(&self) -> bool {
        false
    }
    fn release(&self) {
        self.release_instance();
    }
    fn anonymous_name(&self) -> &'static [u8] {
        Self::ANONYMOUS_NAME
    }
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let initial_capacity = output.remaining();
        loop {
            let record = {
                let mut queue = self.queue.lock();
                queue
                    .as_mut()
                    .ok_or(DeviceError::Errno(errno::EBADF))?
                    .pop(output.remaining())
                    .map_err(|_| DeviceError::Errno(errno::EINVAL))?
            };
            match record {
                Some(record) => {
                    // get_one_event 已退休实际 membership；header/name/padding 均在 guard 外 copy。
                    // Linux inotify_read 的 EFAULT 覆盖先前进度，当前记录不会重放。
                    record
                        .copy_with(|bytes| output.write(bytes))
                        .map_err(|_| DeviceError::ReadFault)?;
                }
                None if output.remaining() != initial_capacity => return Ok(()),
                None => chardev::wait_ready(self, POLLIN, nonblocking)?,
            }
        }
    }

    fn poll(&self, events: i16) -> i16 {
        let queue = self.queue.lock();
        match queue.as_ref() {
            Some(queue) if !queue.is_empty() => events & READ_EVENTS,
            Some(_) => 0,
            None => 0x010,
        }
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        if events & READ_EVENTS != 0 {
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
        let pending = {
            let queue = self.queue.lock();
            let queue = queue.as_ref().ok_or(DeviceError::Errno(errno::EBADF))?;
            i32::try_from(queue.pending_bytes()).unwrap_or(i32::MAX)
        };
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
    if !inode.allows_notifications() {
        return None;
    }
    Some((inode.filesystem_id(), inode.notification_link_state()?.0))
}

/// 把事件投递给 `identity` 上订阅了 `mask` 的全部 watch。
fn deliver(identity: (usize, u64), mask: u32, cookie: u32, name: &[u8], unlinked: bool) {
    let mut cursor = None;
    loop {
        let next = {
            let registry = REGISTRY.lock();
            registry.by_inode.get(&identity).and_then(|list| {
                let mut entries = match cursor {
                    Some(key) => list.watches.iter_after(&key),
                    None => list.watches.iter(),
                };
                entries.next().map(|(&key, watch)| (key, watch.clone()))
            })
        };
        let Some((key, watch)) = next else {
            break;
        };
        cursor = Some(key);
        let subscribed = watch.mask.load(Ordering::Relaxed);
        if subscribed & mask & ALL_EVENTS == 0 || (unlinked && subscribed & IN_EXCL_UNLINK != 0) {
            continue;
        }
        let Some(instance) = watch.instance.upgrade() else {
            continue;
        };
        instance.enqueue(
            watch.wd,
            mask & (ALL_EVENTS | IN_ISDIR),
            cookie,
            name,
            Some(&watch),
        );
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
    let context = opened.watch_context();
    let unlinked = context
        .as_ref()
        .map_or_else(|| opened.is_unlinked(), |(_, _, unlinked)| *unlinked);
    deliver(identity, mask, 0, b"", unlinked);
    if let Some((parent, name, _)) = context
        && let Some(parent_identity) = identity_of(parent.as_ref())
    {
        deliver(parent_identity, mask, 0, &name, unlinked);
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
        deliver(identity, mask, cookie, name, false);
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
        deliver(identity, mask, 0, b"", false);
    }
}

/// 已失去最后 hard link 的实际 dentry 被回收：投递 DELETE_SELF，然后撤销 marks 并投递 IGNORED。
fn notify_removed(identity: (usize, u64), is_directory: bool) {
    if !watching() {
        return;
    }
    deliver(
        identity,
        IN_DELETE_SELF | if is_directory { IN_ISDIR } else { 0 },
        0,
        b"",
        false,
    );
    revoke_all(|candidate| candidate == identity, 0);
}

/// Linux dentry_unlink_inode 的实际 last-dput 通知；仍有其他 hard link 时保留 inode watch。
/// 匿名 dentry 不调用此路径。identity/link count 从实际 storage owner 无分配取得，不取全量 stat 或 page statistics。
pub(crate) fn notify_dentry_reclaimed(inode: &dyn Inode) {
    if !watching() || !inode.allows_notifications() {
        return;
    }
    let Some((number, links)) = inode.notification_link_state() else {
        return;
    };
    if links == 0 {
        notify_removed(
            (inode.filesystem_id(), number),
            inode.inode_type() == InodeType::Directory,
        );
    }
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
    let mut cursor = None;
    loop {
        let next = {
            let registry = REGISTRY.lock();
            let mut entries = match cursor {
                Some((identity, _)) => registry.by_inode.iter_from(&identity),
                None => registry.by_inode.iter(),
            };
            entries.find_map(|(&identity, list)| {
                if !matches(identity) {
                    return None;
                }
                let mut watches = match cursor {
                    Some((previous, key)) if previous == identity => list.watches.iter_after(&key),
                    _ => list.watches.iter(),
                };
                watches
                    .next()
                    .map(|(&key, watch)| ((identity, key), watch.clone()))
            })
        };
        let Some((position, watch)) = next else {
            break;
        };
        cursor = Some(position);
        if let Some(instance) = watch.instance.upgrade() {
            instance.revoke(&watch, extra);
        }
    }
}

/// 解析 `inotify_add_watch` 的路径选项。
pub(crate) fn add_watch_flags(mask: u32) -> (bool, bool) {
    (mask & IN_DONT_FOLLOW != 0, mask & IN_ONLYDIR != 0)
}
