use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use spin::Mutex;

use super::{OpenFileDescription, OpenFileKind};

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
    pub(crate) last_ready: u32,
    pub(crate) disabled: bool,
}

struct Interest {
    ofd: Arc<OpenFileDescription>,
    event: EpollEvent,
    last_ready: u32,
    disabled: bool,
}

/// @description epoll interest identity、ET edge history 与 ONESHOT state 的唯一 owner。
pub(crate) struct Epoll {
    // OWNER: one lock commits interest, ET history and ONESHOT disable atomically; separate maps
    // would permit MOD/rearm to race delivery and either lose or duplicate an event.
    interests: Mutex<BTreeMap<usize, Interest>>,
}

impl Epoll {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            interests: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn change(
        &self,
        operation: EpollChange,
        fd: usize,
        ofd: Option<Arc<OpenFileDescription>>,
        event: Option<EpollEvent>,
    ) -> Result<(), EpollChangeError> {
        let mut interests = self.interests.lock();
        match operation {
            EpollChange::Add => {
                if interests.contains_key(&fd) {
                    return Err(EpollChangeError::Exists);
                }
                let ofd = ofd.ok_or(EpollChangeError::Invalid)?;
                if matches!(ofd.kind, OpenFileKind::Epoll(_)) {
                    return Err(EpollChangeError::Invalid);
                }
                interests.insert(
                    fd,
                    Interest {
                        ofd,
                        event: event.ok_or(EpollChangeError::Invalid)?,
                        last_ready: 0,
                        disabled: false,
                    },
                );
            }
            EpollChange::Delete => {
                interests.remove(&fd).ok_or(EpollChangeError::NotFound)?;
            }
            EpollChange::Modify => {
                let interest = interests.get_mut(&fd).ok_or(EpollChangeError::NotFound)?;
                interest.event = event.ok_or(EpollChangeError::Invalid)?;
                interest.last_ready = 0;
                interest.disabled = false;
            }
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Result<Vec<EpollInterest>, ()> {
        let interests = self.interests.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(interests.len())
            .map_err(|_| ())?;
        snapshot.extend(interests.iter().map(|(fd, interest)| EpollInterest {
            fd: *fd,
            ofd: interest.ofd.clone(),
            event: interest.event,
            last_ready: interest.last_ready,
            disabled: interest.disabled,
        }));
        Ok(snapshot)
    }

    pub(crate) fn commit_delivery(&self, fd: usize, ready: u32, edge: bool, oneshot: bool) {
        if let Some(interest) = self.interests.lock().get_mut(&fd) {
            interest.last_ready = if edge { ready } else { 0 };
            if oneshot {
                interest.disabled = true;
            }
        }
    }

    pub(crate) fn clear_absent_edges(&self, readiness: &[(usize, u32)]) {
        let mut interests = self.interests.lock();
        for (fd, ready) in readiness {
            if *ready == 0
                && let Some(interest) = interests.get_mut(fd)
            {
                interest.last_ready = 0;
            }
        }
    }

    pub(crate) fn remove_ofd(&self, closed: &Arc<OpenFileDescription>) {
        self.interests
            .lock()
            .retain(|_, interest| !Arc::ptr_eq(&interest.ofd, closed));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpollChangeError {
    Exists,
    NotFound,
    Invalid,
}
