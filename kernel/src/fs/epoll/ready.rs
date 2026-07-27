use alloc::{sync::Arc, vec::Vec};

use super::{EPOLL_EDGE, Epoll, EpollInterest, EpollState, InterestKey, OpenFileDescription};

impl Epoll {
    fn refresh_with_events(state: &mut EpollState, key: InterestKey, current: Option<u32>) -> bool {
        let Some(interest) = state.interests.get(&key) else {
            return false;
        };
        let generation = interest
            .ofd
            .readiness_generation(interest.event.events as i16);
        let should_be_ready = !interest.disabled
            && current.is_none_or(|events| events != 0)
            && (interest.event.events & EPOLL_EDGE == 0
                || interest
                    .last_generation
                    .is_none_or(|delivered| generation != delivered));
        let is_ready = state.ready.contains_key(&key);
        if should_be_ready && !is_ready {
            let node = state
                .interests
                .get_mut(&key)
                .unwrap()
                .ready_node
                .take()
                .expect("interest lost preallocated ready node");
            state.ready.commit_vacant(node);
        } else if !should_be_ready && is_ready {
            let node = state.ready.take_entry(&key).unwrap();
            state.interests.get_mut(&key).unwrap().ready_node = Some(node);
        }
        if should_be_ready {
            state.ready_generation = generation;
        }
        should_be_ready && current.is_some()
    }

    pub(super) fn refresh_locked(state: &mut EpollState, key: InterestKey) {
        let Some(interest) = state.interests.get(&key) else {
            return;
        };
        let current = interest.ofd.poll_events(interest.event.events as i16) as u32;
        Self::refresh_with_events(state, key, Some(current));
    }

    /// @description source notifier 持 epoll owner 时无等待地刷新单个 ready membership。
    /// @param state 当前 epoll 的唯一 state guard。
    /// @param key 被 source index 精确路由的 interest。
    /// @return 已取得精确 backend 快照且确认 ready 时为 true；竞争时保守入队但返回 false。
    /// @errors 不分配、不睡眠；`None` 快照由 task-context delivery 精确复查。
    pub(super) fn refresh_source_locked(state: &mut EpollState, key: InterestKey) -> bool {
        let Some(interest) = state.interests.get(&key) else {
            return false;
        };
        let current = interest
            .ofd
            .try_poll_events(interest.event.events as i16)
            .map(|events| events as u32);
        Self::refresh_with_events(state, key, current)
    }

    /// @description 只复制当前 ready memberships，不扫描全部 interests。
    pub(crate) fn ready_snapshot(&self, maximum: usize) -> Result<Vec<EpollInterest>, ()> {
        let mut state = self.state.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(maximum.min(state.ready.len()))
            .map_err(|_| ())?;
        let initial_ready = state.ready.len();
        let start = state.delivery_cursor;
        let mut cursor = start;
        let mut wrapped = start.is_none();
        for _ in 0..initial_ready {
            let mut next = match cursor {
                Some(cursor) => state.ready.successor(&cursor).map(|(key, _)| *key),
                None => state.ready.first_key_value().map(|(key, _)| *key),
            };
            if next.is_none() && !wrapped {
                wrapped = true;
                next = state.ready.first_key_value().map(|(key, _)| *key);
            }
            let Some(key) = next else { break };
            if wrapped && start.is_some_and(|start| key > start) {
                break;
            }
            cursor = Some(key);
            let interest = state.interests.get(&key).unwrap();
            let ready_events = interest.ofd.poll_events(interest.event.events as i16) as u32;
            if ready_events == 0 {
                let node = state.ready.take_entry(&key).unwrap();
                state.interests.get_mut(&key).unwrap().ready_node = Some(node);
                continue;
            }
            snapshot.push(EpollInterest {
                fd: key.fd,
                ofd: interest.ofd.clone(),
                event: interest.event,
                ready_events,
                generation: interest
                    .ofd
                    .readiness_generation(interest.event.events as i16),
                revision: interest.revision,
            });
            if snapshot.len() == maximum {
                break;
            }
        }
        Ok(snapshot)
    }

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
        state.delivery_cursor = Some(key);
        let Some(interest) = state.interests.get(&key) else {
            return;
        };
        if interest.revision != revision || !Arc::ptr_eq(&interest.ofd, ofd) {
            return;
        }
        if let Some(node) = state.ready.take_entry(&key) {
            state.interests.get_mut(&key).unwrap().ready_node = Some(node);
        }
        let interest = state.interests.get_mut(&key).unwrap();
        if edge {
            interest.last_generation = Some(generation);
        }
        if oneshot {
            interest.disabled = true;
        }
        if !edge && !oneshot {
            Self::refresh_locked(&mut state, key);
        }
    }

    pub(crate) fn has_ready(&self) -> bool {
        !self.state.lock().ready.is_empty()
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        let state = self.state.lock();
        if state.ready.is_empty() {
            0
        } else {
            state.ready_generation
        }
    }
}
