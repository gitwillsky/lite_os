use alloc::sync::{Arc, Weak};
use spin::Mutex;

use crate::{
    fallible_tree::FallibleMap,
    fs::{TimerError, TimerFdBackend, TimerFdRead, TimerSetting},
    ipc::{Pipe, PipeEnd},
};

use super::{TimerIdentity, TimerQueue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerFileClock {
    Realtime,
    Monotonic,
    Boottime,
}

pub(super) struct TimerFile {
    target: Weak<dyn TimerFdBackend>,
    clock: TimerFileClock,
    next_expiration_ns: Option<u64>,
    interval_ns: u64,
}

impl TimerFile {
    fn snapshot(&self, now_ns: u64) -> TimerSetting {
        TimerSetting {
            remaining_ns: self
                .next_expiration_ns
                .map_or(0, |expiration| expiration.saturating_sub(now_ns)),
            interval_ns: self.interval_ns,
        }
    }
}

/// Linux timerfd 的 OFD-owned expiration counter 与 readiness endpoint。
struct TimerFd {
    object_id: u64,
    expirations: Mutex<u64>,
    read_notify: Arc<PipeEnd>,
    read_signal: Arc<PipeEnd>,
}

impl TimerFd {
    fn new(read_pair: (Arc<PipeEnd>, Arc<PipeEnd>)) -> Result<Arc<Self>, TimerError> {
        Arc::try_new(Self {
            object_id: crate::id::next_runtime_object_id(),
            expirations: Mutex::new(0),
            read_notify: read_pair.0,
            read_signal: read_pair.1,
        })
        .map_err(|_| TimerError::OutOfMemory)
    }
}

impl TimerFdBackend for TimerFd {
    fn object_id(&self) -> u64 {
        self.object_id
    }

    fn replace(
        &self,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
    ) -> Result<TimerSetting, TimerError> {
        // 单锁把 queue replacement 与 counter reset 排成一个 operation；缺失该锁时 concurrent
        // expiry 可在新 schedule 提交后被旧 reset 擦除，或把旧 schedule 的 tick 泄漏给新 setting。
        let mut expirations = self.expirations.lock();
        let previous = set_timer_file(self.object_id, value_ns, interval_ns, absolute, now_ns)?;
        *expirations = 0;
        self.read_notify.drain_readiness();
        Ok(previous)
    }

    fn setting(&self, now_ns: u64) -> Result<TimerSetting, TimerError> {
        timer_file(self.object_id, now_ns)
    }

    fn read(&self) -> TimerFdRead {
        let mut expirations = self.expirations.lock();
        if *expirations == 0 {
            return TimerFdRead::Empty;
        }
        let value = *expirations;
        *expirations = 0;
        self.read_notify.drain_readiness();
        TimerFdRead::Expirations(value)
    }

    fn readable(&self) -> bool {
        *self.expirations.lock() != 0
    }

    fn notification_pipe(&self) -> Arc<Pipe> {
        self.read_notify.pipe()
    }

    fn readiness_generation(&self) -> u64 {
        self.read_notify
            .pipe()
            .readiness_generation(crate::ipc::PipeDirection::Read)
    }

    fn expire(&self, elapsed: u64) {
        let became_readable = {
            let mut expirations = self.expirations.lock();
            let became_readable = *expirations == 0;
            *expirations = expirations.saturating_add(elapsed);
            became_readable
        };
        if became_readable {
            self.read_signal.signal_readiness();
        }
    }
}

impl Drop for TimerFd {
    fn drop(&mut self) {
        remove_timer_file(self.object_id);
    }
}

impl TimerQueue {
    pub(super) fn pop_expired_timer_file(
        &mut self,
        id: u64,
        expiration: u64,
        mut deadline: crate::fallible_tree::VacantEntry<(u64, TimerIdentity), ()>,
        now_ns: u64,
    ) -> super::ExpiredTimer {
        let timer = self
            .timer_files
            .get_mut(&id)
            .expect("timerfd deadline lost record");
        assert_eq!(timer.next_expiration_ns, Some(expiration));
        let target = timer.target.upgrade();
        let (next, elapsed) = super::next_period(expiration, timer.interval_ns, now_ns);
        timer.next_expiration_ns = next;
        if target.is_some()
            && let Some(next) = next
        {
            deadline.set_key((next, TimerIdentity::File(id)));
            self.deadline_index.commit_vacant(deadline);
        }
        match target {
            Some(target) => super::ExpiredTimer::File { target, elapsed },
            None => {
                self.timer_files.remove(&id);
                super::ExpiredTimer::Silent
            }
        }
    }

    fn take_timer_file(&mut self, id: u64) -> Option<TimerFile> {
        let timer = self.timer_files.remove(&id)?;
        if let Some(expiration) = timer.next_expiration_ns {
            assert!(
                self.deadline_index
                    .remove(&(expiration, TimerIdentity::File(id)))
                    .is_some()
            );
        }
        Some(timer)
    }

    fn replace_timer_file(
        &mut self,
        id: u64,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
        deadline_node: Option<crate::fallible_tree::VacantEntry<(u64, TimerIdentity), ()>>,
    ) -> Result<Option<TimerSetting>, TimerError> {
        let current = self.timer_files.get(&id).ok_or(TimerError::NotFound)?;
        let previous = current.snapshot(now_ns);
        let next = (value_ns != 0).then(|| {
            if !absolute {
                now_ns.saturating_add(value_ns)
            } else if current.clock == TimerFileClock::Realtime {
                crate::timer::realtime_deadline_to_monotonic_ns(value_ns)
            } else {
                value_ns
            }
        });
        if current.next_expiration_ns.is_none() && next.is_some() && deadline_node.is_none() {
            return Ok(None);
        }
        let identity = TimerIdentity::File(id);
        let mut deadline = current.next_expiration_ns.map(|expiration| {
            self.deadline_index
                .take_entry(&(expiration, identity))
                .expect("timerfd record lost deadline index")
        });
        let timer = self
            .timer_files
            .get_mut(&id)
            .expect("timerfd disappeared under owner lock");
        timer.next_expiration_ns = next;
        timer.interval_ns = if next.is_some() { interval_ns } else { 0 };
        if let Some(expiration) = next {
            let mut entry = deadline
                .take()
                .or(deadline_node)
                .expect("armed timerfd deadline node was not prepared");
            entry.set_key((expiration, identity));
            self.deadline_index.commit_vacant(entry);
        }
        Ok(Some(previous))
    }

    fn timer_file(&self, id: u64, now_ns: u64) -> Result<TimerSetting, TimerError> {
        self.timer_files
            .get(&id)
            .map(|timer| timer.snapshot(now_ns))
            .ok_or(TimerError::NotFound)
    }

    fn timer_file_deadline_needed(
        &self,
        id: u64,
        replacement_armed: bool,
    ) -> Result<bool, TimerError> {
        self.timer_files
            .get(&id)
            .map(|timer| timer.next_expiration_ns.is_none() && replacement_armed)
            .ok_or(TimerError::NotFound)
    }
}

/// @description 创建一个注册到 task timer queue 的 Linux timerfd backend。
///
/// @param clock `timerfd_create` 选定且终身不变的 clock domain。
/// @return 可由 fs OFD 持有的 backend。
/// @errors notification、owner 或 registry node 分配失败。
pub(crate) fn create_timer_fd(
    clock: TimerFileClock,
) -> Result<Arc<dyn TimerFdBackend>, TimerError> {
    let pair =
        super::super::create_notification_endpoints().map_err(|()| TimerError::OutOfMemory)?;
    let timer = TimerFd::new(pair)?;
    let id = timer.object_id;
    let backend: Arc<dyn TimerFdBackend> = timer.clone();
    let prepared = FallibleMap::try_prepare(
        id,
        TimerFile {
            target: Arc::downgrade(&backend),
            clock,
            next_expiration_ns: None,
            interval_ns: 0,
        },
    )
    .map_err(|_| TimerError::OutOfMemory)?;
    let mut timers = super::super::TASK_MANAGER.timers.lock();
    if timers.timer_files.contains_key(&id) {
        return Err(TimerError::Exhausted);
    }
    timers.timer_files.commit_vacant(prepared);
    drop(timers);
    Ok(timer)
}

fn set_timer_file(
    id: u64,
    value_ns: u64,
    interval_ns: u64,
    absolute: bool,
    now_ns: u64,
) -> Result<TimerSetting, TimerError> {
    let mut deadline_node = None;
    loop {
        let needed = super::super::TASK_MANAGER
            .timers
            .lock()
            .timer_file_deadline_needed(id, value_ns != 0)?;
        if needed && deadline_node.is_none() {
            deadline_node = Some(
                FallibleMap::try_prepare((0, TimerIdentity::File(id)), ())
                    .map_err(|_| TimerError::OutOfMemory)?,
            );
        }
        if let Some(previous) = super::super::TASK_MANAGER
            .timers
            .lock()
            .replace_timer_file(
                id,
                value_ns,
                interval_ns,
                absolute,
                now_ns,
                deadline_node.take(),
            )?
        {
            return Ok(previous);
        }
    }
}

fn timer_file(id: u64, now_ns: u64) -> Result<TimerSetting, TimerError> {
    super::super::TASK_MANAGER
        .timers
        .lock()
        .timer_file(id, now_ns)
}

fn remove_timer_file(id: u64) {
    super::super::TASK_MANAGER.timers.lock().take_timer_file(id);
}
