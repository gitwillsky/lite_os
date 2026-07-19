use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::{Mutex, Once};

use super::{OpenFileDescription, OpenFileKind, ReadinessSource, ReadinessSources};
use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    ipc::{Pipe, PipeEnd},
};

#[path = "epoll/ready.rs"]
mod ready;

const MAX_NESTING_DEPTH: usize = 5;
const EPOLL_EXCLUSIVE: u32 = 1 << 28;
const EPOLL_EDGE: u32 = 1 << 31;

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
    pub(crate) ready_events: u32,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceIndexKey {
    source: ReadinessSource,
    epoll_identity: usize,
    interest: InterestKey,
}

struct SourceMembership {
    epoll: Weak<Epoll>,
    revision: u64,
    exclusive: bool,
}

type SourceNode = VacantEntry<SourceIndexKey, SourceMembership>;
type ReadyNode = VacantEntry<InterestKey, ()>;

struct Interest {
    ofd: Arc<OpenFileDescription>,
    event: EpollEvent,
    // None 表示 ET 尚未交付；不能用 0 作 sentinel，因为无异步 source 的
    // 同步 ready generation 合法地为 0，否则首个 edge 会被误丢弃。
    last_generation: Option<u64>,
    revision: u64,
    disabled: bool,
    sources: ReadinessSources,
    source_nodes: [Option<SourceNode>; 2],
    ready_node: Option<ReadyNode>,
}

struct EpollState {
    interests: FallibleMap<InterestKey, Interest>,
    ready: FallibleMap<InterestKey, ()>,
    next_revision: u64,
    delivery_cursor: Option<InterestKey>,
    ready_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReverseKey {
    epoll_identity: usize,
    fd: usize,
}

struct ReverseMembership {
    epoll: Weak<Epoll>,
    interest: InterestKey,
}

/// @description OFD 拥有的 epoll reverse-membership index。
///
/// ADD 在 interest 可观察前预分配并发布节点；最后 close 只取出该 OFD
/// 的精确 memberships。缺失该 owner 会退化成 P×E 全局扫描。
pub(crate) struct EpollMemberships {
    entries: Mutex<FallibleMap<ReverseKey, ReverseMembership>>,
}

impl EpollMemberships {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Mutex::new(FallibleMap::new()),
        }
    }

    fn take_first(&self) -> Option<ReverseMembership> {
        let mut entries = self.entries.lock();
        let key = *entries.first_key_value()?.0;
        entries.remove(&key)
    }
}

// OWNER: 持久 source index 将 Pipe/console edge 精确路由到 interest；节点只在
// epoll_ctl ADD 预分配，wake/refresh 只回收并重用节点。
static SOURCE_INDEX: Mutex<FallibleMap<SourceIndexKey, SourceMembership>> =
    Mutex::new(FallibleMap::new());
// OWNER: fs::Epoll 只用该 weak registry 做嵌套 cycle/depth 验证；close cleanup
// 必须走 OFD reverse memberships，否则恢复 P×E 全局扫描。
static EPOLLS: Once<Mutex<Vec<Weak<Epoll>>>> = Once::new();
// OWNER: 串行化 ctl、source rebind 与 final-close graph mutation，避免并发 ADD 越过 cycle check。
static EPOLL_GRAPH: Mutex<()> = Mutex::new(());

/// @description epoll interest、ready membership、ET generation 与 ONESHOT state 的唯一 owner。
pub(crate) struct Epoll {
    state: Mutex<EpollState>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
}

impl Epoll {
    pub(crate) fn identity(epoll: &Arc<Self>) -> usize {
        Arc::as_ptr(epoll) as usize
    }

    pub(crate) fn new(
        notification_read: Arc<PipeEnd>,
        notification_write: Arc<PipeEnd>,
    ) -> Result<Arc<Self>, ()> {
        let epoll = Arc::try_new(Self {
            state: Mutex::new(EpollState {
                interests: FallibleMap::new(),
                ready: FallibleMap::new(),
                next_revision: 1,
                delivery_cursor: None,
                ready_generation: 0,
            }),
            notification_read,
            notification_write,
        })
        .map_err(|_| ())?;
        let _graph = EPOLL_GRAPH.lock();
        let mut registry = EPOLLS.call_once(|| Mutex::new(Vec::new())).lock();
        registry.retain(|entry| entry.strong_count() != 0);
        registry.try_reserve(1).map_err(|_| ())?;
        registry.push(Arc::downgrade(&epoll));
        Ok(epoll)
    }

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
            EpollChange::Add => self.add(key, fd, ofd, event.ok_or(EpollChangeError::Invalid)?)?,
            EpollChange::Delete => {
                if !self.detach(key, &ofd) {
                    return Err(EpollChangeError::NotFound);
                }
            }
            EpollChange::Modify => {
                self.modify(key, &ofd, event.ok_or(EpollChangeError::Invalid)?)?
            }
        }
        drop(_graph);
        self.notify_change();
        Ok(())
    }

    fn add(
        self: &Arc<Self>,
        key: InterestKey,
        fd: usize,
        ofd: Arc<OpenFileDescription>,
        event: EpollEvent,
    ) -> Result<(), EpollChangeError> {
        if !ofd.epoll_pollable() {
            return Err(EpollChangeError::Permission);
        }
        if let OpenFileKind::Epoll(target) = &ofd.kind {
            if event.events & EPOLL_EXCLUSIVE != 0 || Arc::ptr_eq(self, target) {
                return Err(EpollChangeError::Invalid);
            }
            self.validate_nested_add(target)?;
        }
        let revision = {
            let state = self.state.lock();
            if state.interests.contains_key(&key) {
                return Err(EpollChangeError::Exists);
            }
            state.next_revision
        };
        // interest、ready、两个 source 和 reverse 节点在发布前一次预分配。
        // 任一分配失败都返回 ENOMEM，不留半发布 membership。
        let ready_node =
            FallibleMap::try_prepare(key, ()).map_err(|_| EpollChangeError::NoMemory)?;
        let dummy_source = SourceIndexKey {
            source: ReadinessSource::Console,
            epoll_identity: Self::identity(self),
            interest: key,
        };
        let prepare_source = || {
            FallibleMap::try_prepare(
                dummy_source,
                SourceMembership {
                    epoll: Arc::downgrade(self),
                    revision,
                    exclusive: false,
                },
            )
        };
        let source_nodes = [
            Some(prepare_source().map_err(|_| EpollChangeError::NoMemory)?),
            Some(prepare_source().map_err(|_| EpollChangeError::NoMemory)?),
        ];
        let reverse = FallibleMap::try_prepare(
            ReverseKey {
                epoll_identity: Self::identity(self),
                fd,
            },
            ReverseMembership {
                epoll: Arc::downgrade(self),
                interest: key,
            },
        )
        .map_err(|_| EpollChangeError::NoMemory)?;
        let sources = ofd.readiness_sources(event.events as i16);
        let prepared = FallibleMap::try_prepare(
            key,
            Interest {
                ofd: ofd.clone(),
                event,
                last_generation: None,
                revision,
                disabled: false,
                sources,
                source_nodes,
                ready_node: Some(ready_node),
            },
        )
        .map_err(|_| EpollChangeError::NoMemory)?;
        let mut state = self.state.lock();
        state.next_revision = state.next_revision.wrapping_add(1);
        state.interests.commit_vacant(prepared);
        ofd.epoll_memberships.entries.lock().commit_vacant(reverse);
        Self::publish_sources(self, key, state.interests.get_mut(&key).unwrap());
        Self::refresh_locked(&mut state, key);
        Ok(())
    }

    fn modify(
        self: &Arc<Self>,
        key: InterestKey,
        ofd: &Arc<OpenFileDescription>,
        event: EpollEvent,
    ) -> Result<(), EpollChangeError> {
        let mut state = self.state.lock();
        let revision = state.next_revision;
        state.next_revision = state.next_revision.wrapping_add(1);
        let interest = state
            .interests
            .get_mut(&key)
            .filter(|interest| Arc::ptr_eq(&interest.ofd, ofd))
            .ok_or(EpollChangeError::NotFound)?;
        if interest.event.events & EPOLL_EXCLUSIVE != 0 {
            return Err(EpollChangeError::Invalid);
        }
        Self::unpublish_sources(Self::identity(self), key, interest);
        interest.event = event;
        interest.last_generation = None;
        interest.revision = revision;
        interest.disabled = false;
        interest.sources = interest.ofd.readiness_sources(event.events as i16);
        Self::publish_sources(self, key, interest);
        Self::refresh_locked(&mut state, key);
        Ok(())
    }

    fn detach(&self, key: InterestKey, ofd: &Arc<OpenFileDescription>) -> bool {
        let mut state = self.state.lock();
        if state
            .interests
            .get(&key)
            .is_none_or(|interest| !Arc::ptr_eq(&interest.ofd, ofd))
        {
            return false;
        }
        if let Some(node) = state.ready.take_entry(&key) {
            state.interests.get_mut(&key).unwrap().ready_node = Some(node);
        }
        let mut interest = state.interests.remove(&key).unwrap();
        Self::unpublish_sources(self as *const Self as usize, key, &mut interest);
        ofd.epoll_memberships.entries.lock().remove(&ReverseKey {
            epoll_identity: self as *const Self as usize,
            fd: key.fd,
        });
        true
    }

    fn publish_sources(epoll: &Arc<Self>, key: InterestKey, interest: &mut Interest) {
        let mut index = SOURCE_INDEX.lock();
        for (slot, source) in interest.sources.iter().enumerate() {
            let mut node = interest.source_nodes[slot]
                .take()
                .expect("interest lost preallocated source node");
            node.set_key(SourceIndexKey {
                source,
                epoll_identity: Self::identity(epoll),
                interest: key,
            });
            *node.value_mut() = SourceMembership {
                epoll: Arc::downgrade(epoll),
                revision: interest.revision,
                exclusive: interest.event.events & EPOLL_EXCLUSIVE != 0,
            };
            index.commit_vacant(node);
        }
    }

    fn unpublish_sources(epoll_identity: usize, key: InterestKey, interest: &mut Interest) {
        let mut index = SOURCE_INDEX.lock();
        for source in interest.sources.iter() {
            let node = index
                .take_entry(&SourceIndexKey {
                    source,
                    epoll_identity,
                    interest: key,
                })
                .expect("published epoll source membership missing");
            let slot = interest
                .source_nodes
                .iter_mut()
                .find(|slot| slot.is_none())
                .expect("interest source node overflow");
            *slot = Some(node);
        }
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.notification_read.pipe()
    }

    pub(crate) fn consume_notifications(&self) -> u64 {
        self.notification_read.drain_readiness()
    }

    pub(crate) fn recheck_changed(&self, snapshot_generation: u64) -> bool {
        self.notification_read.drain_readiness() != snapshot_generation
    }

    /// @description 最后 descriptor close 只消费目标 OFD 的 reverse memberships。
    pub(crate) fn release_file(closed: &Arc<OpenFileDescription>) {
        while let Some(membership) = closed.epoll_memberships.take_first() {
            let Some(epoll) = membership.epoll.upgrade() else {
                continue;
            };
            let _graph = EPOLL_GRAPH.lock();
            let removed = epoll.detach(membership.interest, closed);
            drop(_graph);
            if removed {
                epoll.notify_change();
            }
        }
    }

    /// @description Pipe state mutation 后精确 refresh 其持久 epoll memberships。
    pub(crate) fn notify_pipe_source(pipe: &Arc<Pipe>) {
        for direction in [
            crate::ipc::PipeDirection::Read,
            crate::ipc::PipeDirection::Write,
        ] {
            Self::notify_source(ReadinessSource::pipe(pipe, direction));
        }
    }

    pub(crate) fn notify_console_source() {
        Self::notify_source(ReadinessSource::Console);
    }

    fn notify_source(source: ReadinessSource) {
        let mut cursor: Option<SourceIndexKey> = None;
        let mut exclusive_selected = false;
        loop {
            let next = {
                let index = SOURCE_INDEX.lock();
                let entry = match cursor {
                    Some(key) => index.successor(&key),
                    None => index.ceiling(&SourceIndexKey {
                        source,
                        epoll_identity: 0,
                        interest: InterestKey {
                            fd: 0,
                            ofd_identity: 0,
                        },
                    }),
                };
                entry.and_then(|(key, membership)| {
                    (key.source == source).then(|| {
                        (
                            *key,
                            membership.epoll.clone(),
                            membership.revision,
                            membership.exclusive,
                        )
                    })
                })
            };
            let Some((key, epoll, revision, exclusive)) = next else {
                break;
            };
            cursor = Some(key);
            if exclusive && exclusive_selected {
                continue;
            }
            let Some(epoll) = epoll.upgrade() else {
                continue;
            };
            if epoll.source_changed(key.interest, revision) {
                exclusive_selected |= exclusive;
            }
        }
    }

    fn source_changed(self: &Arc<Self>, key: InterestKey, revision: u64) -> bool {
        let _graph = EPOLL_GRAPH.lock();
        let mut state = self.state.lock();
        let was_ready = state.ready.contains_key(&key);
        let Some(interest) = state.interests.get_mut(&key) else {
            return false;
        };
        if interest.revision != revision {
            return false;
        }
        Self::unpublish_sources(Self::identity(self), key, interest);
        interest.sources = interest.ofd.readiness_sources(interest.event.events as i16);
        Self::publish_sources(self, key, interest);
        Self::refresh_locked(&mut state, key);
        let now_ready = state.ready.contains_key(&key);
        let changed_ready = was_ready || now_ready;
        if changed_ready {
            drop(state);
            drop(_graph);
            self.notify_change();
        }
        now_ready
    }

    fn notify_change(&self) {
        self.notification_write.signal_readiness();
    }

    fn nested_snapshot(&self) -> Result<Vec<Arc<OpenFileDescription>>, EpollChangeError> {
        let state = self.state.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(state.interests.len())
            .map_err(|_| EpollChangeError::NoMemory)?;
        snapshot.extend(
            state
                .interests
                .values()
                .map(|interest| interest.ofd.clone()),
        );
        Ok(snapshot)
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
        for ofd in current.nested_snapshot()? {
            if let OpenFileKind::Epoll(child) = &ofd.kind
                && let Some(found) = Self::distance_to(child, needle, depth + 1)?
            {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn nesting_depth(current: &Self, depth: usize) -> Result<usize, EpollChangeError> {
        let mut maximum = depth;
        for ofd in current.nested_snapshot()? {
            if let OpenFileKind::Epoll(child) = &ofd.kind {
                maximum = maximum.max(Self::nesting_depth(child, depth + 1)?);
            }
        }
        Ok(maximum)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        // 最后 Arc 已消失，任何 source callback 都不能再 upgrade 本 epoll；因此不取 graph
        // lock，避免父 epoll 在 graph transaction 内释放最后一个 nested interest 时自锁。
        // 每个 cleanup 仍通过精确 key 更新两个外部 owner，不扫描全局 registry。
        let identity = self as *const Self as usize;
        let state = self.state.get_mut();
        while let Some((&key, _)) = state.interests.first_key_value() {
            if let Some(node) = state.ready.take_entry(&key) {
                state.interests.get_mut(&key).unwrap().ready_node = Some(node);
            }
            let mut interest = state.interests.remove(&key).unwrap();
            Self::unpublish_sources(identity, key, &mut interest);
            interest
                .ofd
                .epoll_memberships
                .entries
                .lock()
                .remove(&ReverseKey {
                    epoll_identity: identity,
                    fd: key.fd,
                });
        }
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
