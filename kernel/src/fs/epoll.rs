use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::{Mutex, Once};

use super::{OpenFileDescription, OpenFileKind};
use crate::ipc::{Pipe, PipeEnd};

const MAX_NESTING_DEPTH: usize = 5;
const EPOLL_EXCLUSIVE: u32 = 1 << 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpollChange {
    Add,
    Delete,
    Modify,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EpollEvent {
    pub(crate) events: u32,
    pub(crate) data: u64,
}

pub(crate) struct EpollInterest {
    pub(crate) fd: usize,
    pub(crate) ofd: Arc<OpenFileDescription>,
    pub(crate) event: EpollEvent,
    pub(crate) last_generation: u64,
    pub(crate) revision: u64,
    pub(crate) disabled: bool,
}

struct Interest {
    ofd: Arc<OpenFileDescription>,
    event: EpollEvent,
    last_generation: u64,
    revision: u64,
    disabled: bool,
}

impl Interest {
    fn is_ready(&self) -> bool {
        if self.disabled {
            return false;
        }
        let current = self.ofd.poll_events(self.event.events as i16) as u32;
        if current == 0 {
            return false;
        }
        self.event.events & (1 << 31) == 0
            || self.ofd.readiness_generation(self.event.events as i16) != self.last_generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct InterestKey {
    fd: usize,
    ofd_identity: usize,
}

impl InterestKey {
    fn new(fd: usize, ofd: &Arc<OpenFileDescription>) -> Self {
        Self {
            fd,
            ofd_identity: Arc::as_ptr(ofd) as usize,
        }
    }
}

struct EpollState {
    interests: BTreeMap<InterestKey, Interest>,
    next_revision: u64,
    delivery_cursor: Option<InterestKey>,
}

// OWNER: registry 只保留 weak epoll lifetime，用于跨 fork/fd-table 的最后 descriptor close
// 清理。若只扫描当前 fd table，另一个进程关闭最后引用后会留下可被 fd reuse 命中的 interest。
static EPOLLS: Once<Mutex<Vec<Weak<Epoll>>>> = Once::new();
// OWNER: fs::Epoll 唯一拥有嵌套图 mutation serialization；所有图变更与 close cleanup 都先取此锁。
// 所有嵌套图变更和最后引用清理遵循 graph -> registry -> epoll state 的锁序；缺少全局图锁会
// 让两个并发 ADD 都在旧图上通过 cycle check，随后共同构成环并令 readiness 递归不终止。
static EPOLL_GRAPH: Mutex<()> = Mutex::new(());

/// @description epoll interest、ET generation 与 ONESHOT state 的唯一 owner。
pub(crate) struct Epoll {
    // 同一把锁原子提交 interest identity、MOD revision、ET generation 和 ONESHOT disable。
    state: Mutex<EpollState>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
}

impl Epoll {
    /// @description 返回 source wake 用于合并同一 epoll instance waiters 的稳定 identity。
    ///
    /// @param epoll live epoll Arc。
    /// @return Arc allocation lifetime 内稳定的地址 identity。
    pub(crate) fn identity(epoll: &Arc<Self>) -> usize {
        Arc::as_ptr(epoll) as usize
    }

    /// @description 创建并注册一个只由 weak registry 跟踪的 epoll instance。
    ///
    /// @param notification_read 绑定统一 Poll registry 的内部 read endpoint。
    /// @param notification_write ctl/close mutation 的内部 wake endpoint。
    /// @return 新 epoll owner；registry 扩容失败返回错误。
    pub(crate) fn new(
        notification_read: Arc<PipeEnd>,
        notification_write: Arc<PipeEnd>,
    ) -> Result<Arc<Self>, ()> {
        let epoll = Arc::new(Self {
            state: Mutex::new(EpollState {
                interests: BTreeMap::new(),
                next_revision: 1,
                delivery_cursor: None,
            }),
            notification_read,
            notification_write,
        });
        let _graph = EPOLL_GRAPH.lock();
        let mut registry = EPOLLS.call_once(|| Mutex::new(Vec::new())).lock();
        registry.retain(|entry| entry.strong_count() != 0);
        registry.try_reserve(1).map_err(|_| ())?;
        registry.push(Arc::downgrade(&epoll));
        Ok(epoll)
    }

    /// @description 按 Linux fd+OFD identity 原子修改 interest。
    ///
    /// @param operation ADD、DEL 或 MOD。
    /// @param fd 注册时的 descriptor number。
    /// @param ofd 本次 epoll_ctl 解析得到的 live OFD identity。
    /// @param event ADD/MOD 的 event payload。
    /// @return 修改成功，或精确的 identity、容量、pollability、嵌套错误。
    pub(crate) fn change(
        self: &Arc<Self>,
        operation: EpollChange,
        fd: usize,
        ofd: Arc<OpenFileDescription>,
        event: Option<EpollEvent>,
    ) -> Result<(), EpollChangeError> {
        let _graph = EPOLL_GRAPH.lock();
        let key = InterestKey::new(fd, &ofd);
        match operation {
            EpollChange::Add => {
                if !ofd.epoll_pollable() {
                    return Err(EpollChangeError::Permission);
                }
                let event = event.ok_or(EpollChangeError::Invalid)?;
                if let OpenFileKind::Epoll(target) = &ofd.kind {
                    if event.events & EPOLL_EXCLUSIVE != 0 {
                        return Err(EpollChangeError::Invalid);
                    }
                    if Arc::ptr_eq(self, target) {
                        return Err(EpollChangeError::Invalid);
                    }
                    self.validate_nested_add(target)?;
                }
                let mut state = self.state.lock();
                if state.interests.contains_key(&key) {
                    return Err(EpollChangeError::Exists);
                }
                let revision = state.next_revision;
                state.next_revision = state.next_revision.wrapping_add(1);
                state.interests.insert(
                    key,
                    Interest {
                        ofd,
                        event,
                        last_generation: 0,
                        revision,
                        disabled: false,
                    },
                );
            }
            EpollChange::Delete => {
                let mut state = self.state.lock();
                let interest = state
                    .interests
                    .get(&key)
                    .ok_or(EpollChangeError::NotFound)?;
                if !Arc::ptr_eq(&interest.ofd, &ofd) {
                    return Err(EpollChangeError::NotFound);
                }
                state.interests.remove(&key);
            }
            EpollChange::Modify => {
                let mut state = self.state.lock();
                let revision = state.next_revision;
                state.next_revision = state.next_revision.wrapping_add(1);
                let interest = state
                    .interests
                    .get_mut(&key)
                    .ok_or(EpollChangeError::NotFound)?;
                if !Arc::ptr_eq(&interest.ofd, &ofd) {
                    return Err(EpollChangeError::NotFound);
                }
                if interest.event.events & EPOLL_EXCLUSIVE != 0 {
                    return Err(EpollChangeError::Invalid);
                }
                interest.event = event.ok_or(EpollChangeError::Invalid)?;
                interest.last_generation = 0;
                interest.revision = revision;
                interest.disabled = false;
            }
        }
        self.notify_change();
        Ok(())
    }

    /// @description 复制一个带 revision 的 interest 快照，供无锁 user-copy 阶段使用。
    ///
    /// @return 成功返回快照；kernel heap 不足返回错误。
    pub(crate) fn snapshot(&self) -> Result<Vec<EpollInterest>, ()> {
        let state = self.state.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(state.interests.len())
            .map_err(|_| ())?;
        snapshot.extend(state.interests.iter().map(|(key, interest)| EpollInterest {
            fd: key.fd,
            ofd: interest.ofd.clone(),
            event: interest.event,
            last_generation: interest.last_generation,
            revision: interest.revision,
            disabled: interest.disabled,
        }));
        if let Some(cursor) = state.delivery_cursor {
            let split = snapshot
                .iter()
                .position(|interest| InterestKey::new(interest.fd, &interest.ofd) > cursor)
                .unwrap_or(0);
            snapshot.rotate_left(split);
        }
        Ok(snapshot)
    }

    /// @description 在 user-copy 完整成功后提交一次 delivery。
    ///
    /// 并发 MOD/DEL 或 fd reuse 会改变 revision/OFD identity；旧 snapshot 不得禁用或覆盖新 interest。
    pub(crate) fn commit_delivery(
        &self,
        fd: usize,
        ofd: &Arc<OpenFileDescription>,
        revision: u64,
        generation: u64,
        edge: bool,
        oneshot: bool,
    ) {
        let mut state = self.state.lock();
        let key = InterestKey::new(fd, ofd);
        // 每次完整 copyout 后推进 cursor；缺失该状态时，永久 ready 的最小 key 会在
        // maxevents 小于 ready 数量时饿死后续 interest。
        state.delivery_cursor = Some(key);
        let Some(interest) = state.interests.get_mut(&key) else {
            return;
        };
        if interest.revision != revision || !Arc::ptr_eq(&interest.ofd, ofd) {
            return;
        }
        if edge {
            interest.last_generation = generation;
        }
        if oneshot {
            interest.disabled = true;
        }
    }

    /// @description 查询当前是否至少存在一个满足 LT/ET/ONESHOT 状态的 interest。
    ///
    /// @return 存在可交付事件返回 true。
    pub(crate) fn has_ready(&self) -> bool {
        self.state.lock().interests.values().any(Interest::is_ready)
    }

    /// @description 为嵌套 epoll 投影当前可交付子事件的最新全局 generation。
    ///
    /// @return 没有可交付事件返回零，否则返回最新 source generation。
    pub(crate) fn readiness_generation(&self) -> u64 {
        self.state
            .lock()
            .interests
            .values()
            .filter(|interest| interest.is_ready())
            .map(|interest| {
                interest
                    .ofd
                    .readiness_generation(interest.event.events as i16)
            })
            .max()
            .unwrap_or(0)
    }

    /// @description 返回用于唤醒旧 wait-key snapshot 的内部 notification pipe。
    ///
    /// @return read 方向 Pipe identity；只暴露给统一 Poll wait registration。
    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.notification_read.pipe()
    }

    /// @description 在重新求值前排空已消费的 ctl/close notification。
    pub(crate) fn consume_notifications(&self) {
        self.notification_read.drain_readiness();
    }

    /// @description 最后一个 descriptor 引用消失时，从所有 live epoll 删除目标 OFD。
    pub(crate) fn release_file(closed: &Arc<OpenFileDescription>) {
        let _graph = EPOLL_GRAPH.lock();
        let mut registry = EPOLLS.call_once(|| Mutex::new(Vec::new())).lock();
        registry.retain(|entry| entry.strong_count() != 0);
        for entry in registry.iter() {
            if let Some(epoll) = entry.upgrade() {
                let removed = {
                    let mut state = epoll.state.lock();
                    let previous = state.interests.len();
                    state
                        .interests
                        .retain(|_, interest| !Arc::ptr_eq(&interest.ofd, closed));
                    state.interests.len() != previous
                };
                if removed {
                    epoll.notify_change();
                }
            }
        }
    }

    fn notify_change(&self) {
        self.notification_write.signal_readiness();
    }

    fn live_epolls() -> Result<Vec<Arc<Self>>, EpollChangeError> {
        let mut registry = EPOLLS.call_once(|| Mutex::new(Vec::new())).lock();
        registry.retain(|entry| entry.strong_count() != 0);
        let mut live = Vec::new();
        live.try_reserve_exact(registry.len())
            .map_err(|_| EpollChangeError::NoMemory)?;
        live.extend(registry.iter().filter_map(Weak::upgrade));
        Ok(live)
    }

    fn validate_nested_add(&self, target: &Arc<Self>) -> Result<(), EpollChangeError> {
        if Self::distance_to(target, self, 0)?.is_some() {
            return Err(EpollChangeError::Loop);
        }
        let descendants = Self::nesting_depth(target, 0)?;
        let mut ancestors = 0;
        for root in Self::live_epolls()? {
            if let Some(distance) = Self::distance_to(&root, self, 0)? {
                ancestors = ancestors.max(distance);
            }
        }
        if ancestors + 1 + descendants > MAX_NESTING_DEPTH {
            return Err(EpollChangeError::Loop);
        }
        Ok(())
    }

    fn distance_to(
        current: &Self,
        needle: &Self,
        depth: usize,
    ) -> Result<Option<usize>, EpollChangeError> {
        if core::ptr::eq(current, needle) {
            return Ok(Some(depth));
        }
        if depth == MAX_NESTING_DEPTH {
            return Ok(None);
        }
        for interest in current.snapshot().map_err(|_| EpollChangeError::NoMemory)? {
            if let OpenFileKind::Epoll(child) = &interest.ofd.kind
                && let Some(found) = Self::distance_to(child, needle, depth + 1)?
            {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn nesting_depth(current: &Self, depth: usize) -> Result<usize, EpollChangeError> {
        let mut maximum = depth;
        for interest in current.snapshot().map_err(|_| EpollChangeError::NoMemory)? {
            if let OpenFileKind::Epoll(child) = &interest.ofd.kind {
                maximum = maximum.max(Self::nesting_depth(child, depth + 1)?);
            }
        }
        Ok(maximum)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpollChangeError {
    Exists,
    NotFound,
    Invalid,
    Permission,
    Loop,
    NoMemory,
}
