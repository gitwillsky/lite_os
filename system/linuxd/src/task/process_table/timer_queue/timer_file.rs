use alloc::sync::{Arc, Weak};
use spin::Mutex;

use crate::{
    fallible_tree::FallibleMap,
    file::{TimerError, TimerFdBackend, TimerFdRead, TimerSetting},
    ipc::{Pipe, PipeEnd},
};

use super::{RetiredTimers, TimerExpirationRetirement, TimerIdentity, TimerQueue};

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
    // OWNER: schedule publication 的唯一版本；锁外 expiry 携带历史 scalar，不在 backend 镜像此字段。
    generation: u64,
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
    fn close(&self) {
        remove_timer_file(self.object_id);
        self.read_notify.close();
        self.read_signal.close();
    }
    fn replace(
        &self,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
        offsets: crate::namespace::TimeOffsets,
    ) -> Result<TimerSetting, TimerError> {
        set_timer_file(self, value_ns, interval_ns, absolute, now_ns, offsets)
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

    fn expire(&self, elapsed: u64, generation: u64) {
        let became_readable = {
            let mut expirations = self.expirations.lock();
            // counter→queue 与 replacement 同锁序；队列摘下旧 event 后的 rearm 必须使其失效。
            // queue 只读复查实际 owner 的 generation，不缓存或复制当前版本到 counter owner。
            if !super::super::PROCESS_TABLE
                .timers
                .lock()
                .timer_files
                .get(&self.object_id)
                .is_some_and(|timer| timer.generation == generation)
            {
                return;
            }
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
        self.close();
    }
}

impl TimerQueue {
    pub(super) fn pop_expired_timer_file(
        &mut self,
        id: u64,
        expiration: u64,
        deadline: super::DeadlineEntry,
        now_ns: u64,
    ) -> super::ExpiredTimer {
        let mut deadline = Some(deadline);
        let timer = self
            .timer_files
            .get_mut(&id)
            .expect("timerfd deadline lost record");
        assert_eq!(timer.next_expiration_ns, Some(expiration));
        let target = timer.target.upgrade();
        let generation = timer.generation;
        let (next, elapsed) = super::next_period(expiration, timer.interval_ns, now_ns);
        timer.next_expiration_ns = next;
        if target.is_some()
            && let Some(next) = next
        {
            let mut entry = deadline.take().expect("actual expired file deadline");
            entry.set_key((next, TimerIdentity::File(id)));
            self.deadline_index.commit_vacant(entry);
        }
        let mut retired = TimerExpirationRetirement::from_deadline(deadline);
        match target {
            Some(target) => super::ExpiredTimer::File {
                target,
                elapsed,
                generation,
                retired,
            },
            None => {
                retired.file = Some(
                    self.timer_files
                        .take_entry(&id)
                        .expect("actual retired file timer"),
                );
                super::ExpiredTimer::Silent(retired)
            }
        }
    }

    fn take_timer_file(&mut self, id: u64) -> RetiredTimers {
        let mut retired = RetiredTimers::default();
        if let Some(timer) = self.timer_files.take_entry(&id) {
            if let Some(expiration) = timer.value().next_expiration_ns {
                retired.deadlines.commit_vacant(
                    self.deadline_index
                        .take_entry(&(expiration, TimerIdentity::File(id)))
                        .expect("actual retired file deadline"),
                );
            }
            retired.files.commit_vacant(timer);
        }
        retired
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    fn replace_timer_file(
        &mut self,
        id: u64,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
        offsets: crate::namespace::TimeOffsets,
        deadline_node: &mut Option<super::DeadlineEntry>,
    ) -> Result<Option<(TimerSetting, RetiredTimers)>, TimerError> {
        let current = self.timer_files.get(&id).ok_or(TimerError::NotFound)?;
        let previous = current.snapshot(now_ns);
        let next = (value_ns != 0).then(|| {
            if !absolute {
                now_ns.saturating_add(value_ns).min(i64::MAX as u64)
            } else {
                match current.clock {
                    TimerFileClock::Realtime => {
                        crate::execution::timer::realtime_deadline_to_monotonic_ns(value_ns)
                    }
                    TimerFileClock::Monotonic => {
                        offsets.to_host(crate::namespace::TimeClock::Monotonic, value_ns)
                    }
                    TimerFileClock::Boottime => {
                        offsets.to_host(crate::namespace::TimeClock::Boottime, value_ns)
                    }
                }
            }
        });
        if current.next_expiration_ns.is_none() && next.is_some() && deadline_node.is_none() {
            return Ok(None);
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(TimerError::Exhausted)?;
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
        timer.generation = generation;
        timer.interval_ns = if next.is_some() { interval_ns } else { 0 };
        if let Some(expiration) = next {
            let mut entry = deadline
                .take()
                .or_else(|| deadline_node.take())
                .expect("armed timerfd deadline node was not prepared");
            entry.set_key((expiration, identity));
            self.deadline_index.commit_vacant(entry);
        }
        let mut retired = RetiredTimers::default();
        if let Some(deadline) = deadline {
            retired.deadlines.commit_vacant(deadline);
        }
        Ok(Some((previous, retired)))
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

/// 创建一个注册到 task timer queue 的 Linux timerfd backend。
///
/// # Parameters
///
/// - `clock`: `timerfd_create` 选定且终身不变的 clock domain。
///
/// # Returns
///
/// 可由 fs OFD 持有的 backend。
///
/// # Errors
///
/// notification、owner 或 registry node 分配失败。
pub(crate) fn create_timer_fd(
    clock: TimerFileClock,
) -> Result<Arc<dyn TimerFdBackend>, TimerError> {
    let pair = crate::ipc::Pipe::notification_pair().map_err(|()| TimerError::OutOfMemory)?;
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
            generation: 0,
        },
    )
    .map_err(|_| TimerError::OutOfMemory)?;
    let mut timers = super::super::PROCESS_TABLE.timers.lock();
    if timers.timer_files.contains_key(&id) {
        return Err(TimerError::Exhausted);
    }
    timers.timer_files.commit_vacant(prepared);
    drop(timers);
    Ok(timer)
}

fn set_timer_file(
    timer: &TimerFd,
    value_ns: u64,
    interval_ns: u64,
    absolute: bool,
    now_ns: u64,
    offsets: crate::namespace::TimeOffsets,
) -> Result<TimerSetting, TimerError> {
    let id = timer.object_id;
    let mut deadline_node = None;
    loop {
        let needed = super::super::PROCESS_TABLE
            .timers
            .lock()
            .timer_file_deadline_needed(id, value_ns != 0)?;
        if needed && deadline_node.is_none() {
            deadline_node = Some(
                FallibleMap::try_prepare((0, TimerIdentity::File(id)), None)
                    .map_err(|_| TimerError::OutOfMemory)?,
            );
        }
        // 1. deadline allocation 完成后才取得 counter；2. 同锁序提交实际 queue 和 counter reset。
        // 并发 expiry 使计划过期时，先释放两锁，再准备 node/retry；不能在 spin guard 内分配。
        let mut expirations = timer.expirations.lock();
        let previous = {
            let mut timers = super::super::PROCESS_TABLE.timers.lock();
            timers.replace_timer_file(
                id,
                value_ns,
                interval_ns,
                absolute,
                now_ns,
                offsets,
                &mut deadline_node,
            )?
        };
        if let Some((previous, retired)) = previous {
            *expirations = 0;
            timer.read_notify.drain_readiness();
            drop(expirations);
            drop(retired);
            return Ok(previous);
        }
    }
}

fn timer_file(id: u64, now_ns: u64) -> Result<TimerSetting, TimerError> {
    super::super::PROCESS_TABLE
        .timers
        .lock()
        .timer_file(id, now_ns)
}

fn remove_timer_file(id: u64) {
    let retired = super::super::PROCESS_TABLE
        .timers
        .lock()
        .take_timer_file(id);
    drop(retired);
}
