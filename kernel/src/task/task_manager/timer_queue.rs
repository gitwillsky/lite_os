use crate::fallible_tree::FallibleMap;

mod period;
mod posix_creation;
mod preparation;
mod preparation_policy;
use period::next_period;
use preparation::{PreparedPosixCreate, PreparedPosixReplacement, PreparedRealReplacement};
use preparation_policy::{TimerReplacementNeeds, posix_deadline_needed, real_replacement_needs};

#[derive(Clone, Copy)]
struct RealTimer {
    next_expiration_ns: Option<u64>,
    interval_ns: u64,
}

impl RealTimer {
    fn snapshot(self, now_ns: u64) -> TimerSetting {
        TimerSetting {
            remaining_ns: self
                .next_expiration_ns
                .map_or(0, |expiration| expiration.saturating_sub(now_ns)),
            interval_ns: self.interval_ns,
        }
    }
}

/// POSIX timer 到期时的 Linux signal 路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PosixTimerNotification {
    /// `timer_create(..., NULL, ...)`：SIGALRM，且 `sival_int` 为分配后的 timer ID。
    Default,
    /// `SIGEV_NONE`：推进 timer，但不发布 signal。
    None,
    /// `SIGEV_SIGNAL`：向创建进程发布 signal。
    Process { signal: usize, value: u64 },
    /// `SIGEV_THREAD_ID`：向创建进程中的指定 Thread 发布 signal。
    Thread {
        tid: usize,
        signal: usize,
        value: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PosixTimerClock {
    Realtime,
    Monotonic,
}

#[derive(Clone, Copy)]
struct PosixTimer {
    clock: PosixTimerClock,
    notification: PosixTimerNotification,
    next_expiration_ns: Option<u64>,
    interval_ns: u64,
    overrun: i32,
}

impl PosixTimer {
    fn snapshot(self, now_ns: u64) -> TimerSetting {
        TimerSetting {
            remaining_ns: self
                .next_expiration_ns
                .map_or(0, |expiration| expiration.saturating_sub(now_ns)),
            interval_ns: self.interval_ns,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TimerIdentity {
    Real(usize),
    Posix(usize, i32),
}

/// 一个 timer 在 syscall 边界可观察的相对 setting。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimerSetting {
    pub(crate) remaining_ns: u64,
    pub(crate) interval_ns: u64,
}

/// 锁外 signal delivery 所需的完整 POSIX timer 到期值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExpiredPosixTimer {
    pub(super) tgid: usize,
    pub(super) id: i32,
    pub(super) notification: PosixTimerNotification,
    pub(super) overrun: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExpiredTimer {
    Real(usize),
    Posix(ExpiredPosixTimer),
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerError {
    NotFound,
    OutOfMemory,
    Exhausted,
}

/// ITIMER_REAL、POSIX timer record 与 active deadline index 的唯一复合状态 owner。
pub(super) struct TimerQueue {
    real_timers: FallibleMap<usize, RealTimer>,
    posix_timers: FallibleMap<(usize, i32), PosixTimer>,
    // OWNER: 仅本类型在同一 timer lock 下同步 record 与 active deadline membership。
    // 缺失统一 index 会恢复每 tick O(processes+timers) 扫描；分离写入口会漏发或重复 signal。
    deadline_index: FallibleMap<(u64, TimerIdentity), ()>,
}

impl TimerQueue {
    pub(super) const fn new() -> Self {
        Self {
            real_timers: FallibleMap::new(),
            posix_timers: FallibleMap::new(),
            deadline_index: FallibleMap::new(),
        }
    }

    fn take_real(&mut self, tgid: usize) -> Option<RealTimer> {
        let timer = self.real_timers.remove(&tgid)?;
        if let Some(expiration) = timer.next_expiration_ns {
            assert!(
                self.deadline_index
                    .remove(&(expiration, TimerIdentity::Real(tgid)))
                    .is_some()
            );
        }
        Some(timer)
    }

    fn take_posix(&mut self, key: (usize, i32)) -> Option<PosixTimer> {
        let timer = self.posix_timers.remove(&key)?;
        if let Some(expiration) = timer.next_expiration_ns {
            assert!(
                self.deadline_index
                    .remove(&(expiration, TimerIdentity::Posix(key.0, key.1)))
                    .is_some()
            );
        }
        Some(timer)
    }

    /// 删除 Process exit 拥有的全部 interval/POSIX timer。
    pub(super) fn remove_process(&mut self, tgid: usize) {
        self.take_real(tgid);
        self.remove_posix_timers(tgid);
    }

    /// 删除 exec 不得继承的 POSIX timers，保留 Linux 规定继承的 ITIMER_REAL。
    pub(super) fn remove_posix_timers(&mut self, tgid: usize) {
        let mut cursor = (tgid, -1);
        loop {
            let Some(key) = self
                .posix_timers
                .iter_after(&cursor)
                .next()
                .map(|(&key, _)| key)
                .filter(|key| key.0 == tgid)
            else {
                return;
            };
            self.take_posix(key)
                .expect("selected POSIX timer disappeared under owner lock");
            cursor = key;
        }
    }

    fn replace_real(
        &mut self,
        prepared: PreparedRealReplacement,
        now_ns: u64,
    ) -> Option<TimerSetting> {
        let PreparedRealReplacement {
            tgid,
            replacement,
            timer_node,
            deadline_node,
        } = prepared;
        let next = replacement.and_then(|timer| timer.next_expiration_ns);
        let current = self.real_timers.get(&tgid).copied();
        let needs = real_replacement_needs(
            current.is_some(),
            current.is_some_and(|timer| timer.next_expiration_ns.is_some()),
            replacement.is_some(),
            next.is_some(),
        );
        if needs.record && timer_node.is_none() || needs.deadline && deadline_node.is_none() {
            return None;
        }
        let identity = TimerIdentity::Real(tgid);
        let previous = current.map_or(
            TimerSetting {
                remaining_ns: 0,
                interval_ns: 0,
            },
            |timer| timer.snapshot(now_ns),
        );
        let mut deadline = current.and_then(|timer| {
            timer.next_expiration_ns.map(|expiration| {
                self.deadline_index
                    .take_entry(&(expiration, identity))
                    .expect("ITIMER_REAL record lost deadline index")
            })
        });
        match replacement {
            Some(timer) => {
                if let Some(current) = self.real_timers.get_mut(&tgid) {
                    *current = timer;
                } else {
                    self.real_timers
                        .commit_vacant(timer_node.expect("new real timer node not prepared"));
                }
            }
            None => {
                self.real_timers.remove(&tgid);
            }
        }
        if let Some(expiration) = next {
            let entry = if let Some(mut entry) = deadline.take() {
                entry.set_key((expiration, identity));
                entry
            } else {
                let mut entry = deadline_node.expect("new real deadline node not prepared");
                entry.set_key((expiration, identity));
                entry
            };
            self.deadline_index.commit_vacant(entry);
        }
        Some(previous)
    }

    pub(super) fn real(&self, tgid: usize, now_ns: u64) -> TimerSetting {
        self.real_timers.get(&tgid).copied().map_or(
            TimerSetting {
                remaining_ns: 0,
                interval_ns: 0,
            },
            |timer| timer.snapshot(now_ns),
        )
    }

    fn real_replacement_needs(
        &self,
        tgid: usize,
        replacement_record: bool,
        replacement_deadline: bool,
    ) -> TimerReplacementNeeds {
        let current = self.real_timers.get(&tgid).copied();
        real_replacement_needs(
            current.is_some(),
            current.is_some_and(|timer| timer.next_expiration_ns.is_some()),
            replacement_record,
            replacement_deadline,
        )
    }

    fn replace_posix(
        &mut self,
        prepared: PreparedPosixReplacement,
        now_ns: u64,
    ) -> Result<Option<TimerSetting>, TimerError> {
        let PreparedPosixReplacement {
            key,
            value_ns,
            interval_ns,
            absolute,
            deadline_node,
        } = prepared;
        let current = self
            .posix_timers
            .get(&key)
            .copied()
            .ok_or(TimerError::NotFound)?;
        let identity = TimerIdentity::Posix(key.0, key.1);
        let next = (value_ns != 0).then(|| {
            if !absolute {
                now_ns.saturating_add(value_ns)
            } else if current.clock == PosixTimerClock::Realtime {
                crate::timer::realtime_deadline_to_monotonic_ns(value_ns)
            } else {
                value_ns
            }
        });
        if posix_deadline_needed(current.next_expiration_ns.is_some(), next.is_some())
            && deadline_node.is_none()
        {
            return Ok(None);
        }
        let mut deadline = current.next_expiration_ns.map(|expiration| {
            self.deadline_index
                .take_entry(&(expiration, identity))
                .expect("POSIX timer record lost deadline index")
        });
        let timer = self
            .posix_timers
            .get_mut(&key)
            .expect("POSIX timer disappeared under owner lock");
        let previous = timer.snapshot(now_ns);
        timer.next_expiration_ns = next;
        timer.interval_ns = if next.is_some() { interval_ns } else { 0 };
        timer.overrun = 0;
        if let Some(expiration) = next {
            let entry = if let Some(mut entry) = deadline.take() {
                entry.set_key((expiration, identity));
                entry
            } else {
                let mut entry = deadline_node.expect("new POSIX deadline node not prepared");
                entry.set_key((expiration, identity));
                entry
            };
            self.deadline_index.commit_vacant(entry);
        }
        Ok(Some(previous))
    }

    pub(super) fn posix(
        &self,
        tgid: usize,
        id: i32,
        now_ns: u64,
    ) -> Result<TimerSetting, TimerError> {
        self.posix_timers
            .get(&(tgid, id))
            .copied()
            .map(|timer| timer.snapshot(now_ns))
            .ok_or(TimerError::NotFound)
    }

    fn posix_deadline_needed(
        &self,
        tgid: usize,
        id: i32,
        replacement_deadline: bool,
    ) -> Result<bool, TimerError> {
        self.posix_timers
            .get(&(tgid, id))
            .map(|timer| {
                posix_deadline_needed(timer.next_expiration_ns.is_some(), replacement_deadline)
            })
            .ok_or(TimerError::NotFound)
    }

    pub(super) fn posix_overrun(&self, tgid: usize, id: i32) -> Result<i32, TimerError> {
        self.posix_timers
            .get(&(tgid, id))
            .map(|timer| timer.overrun)
            .ok_or(TimerError::NotFound)
    }

    pub(super) fn delete_posix(&mut self, tgid: usize, id: i32) -> Result<(), TimerError> {
        self.take_posix((tgid, id))
            .map(|_| ())
            .ok_or(TimerError::NotFound)
    }

    pub(super) fn pop_expired(&mut self, now_ns: u64) -> Option<ExpiredTimer> {
        let (&(expiration, identity), _) = self.deadline_index.first_key_value()?;
        if expiration > now_ns {
            return None;
        }
        let mut deadline = self
            .deadline_index
            .take_entry(&(expiration, identity))
            .expect("selected timer deadline disappeared");
        match identity {
            TimerIdentity::Real(tgid) => {
                let timer = self
                    .real_timers
                    .get_mut(&tgid)
                    .expect("ITIMER_REAL deadline lost record");
                assert_eq!(timer.next_expiration_ns, Some(expiration));
                timer.next_expiration_ns = next_period(expiration, timer.interval_ns, now_ns).0;
                if let Some(next) = timer.next_expiration_ns {
                    deadline.set_key((next, identity));
                    self.deadline_index.commit_vacant(deadline);
                }
                Some(ExpiredTimer::Real(tgid))
            }
            TimerIdentity::Posix(tgid, id) => {
                let timer = self
                    .posix_timers
                    .get_mut(&(tgid, id))
                    .expect("POSIX timer deadline lost record");
                assert_eq!(timer.next_expiration_ns, Some(expiration));
                let (next, elapsed) = next_period(expiration, timer.interval_ns, now_ns);
                timer.next_expiration_ns = next;
                timer.overrun = elapsed.saturating_sub(1).min(i32::MAX as u64) as i32;
                if let Some(next) = next {
                    deadline.set_key((next, identity));
                    self.deadline_index.commit_vacant(deadline);
                }
                let notification = match timer.notification {
                    PosixTimerNotification::Default => PosixTimerNotification::Process {
                        signal: 14,
                        value: id as u64,
                    },
                    notification => notification,
                };
                if notification == PosixTimerNotification::None {
                    Some(ExpiredTimer::Silent)
                } else {
                    Some(ExpiredTimer::Posix(ExpiredPosixTimer {
                        tgid,
                        id,
                        notification,
                        overrun: timer.overrun,
                    }))
                }
            }
        }
    }

    pub(super) fn has_expired(&self, now_ns: u64) -> bool {
        self.deadline_index
            .first_key_value()
            .is_some_and(|(&(expiration, _), _)| expiration <= now_ns)
    }
}

fn live_process(
    graph: &super::ProcessGraph,
    tgid: usize,
) -> Result<&FallibleMap<usize, alloc::sync::Arc<crate::task::TaskControlBlock>>, TimerError> {
    match graph.nodes.get(&tgid).map(|node| &node.state) {
        Some(super::ProcessState::Live(threads)) => Ok(threads),
        _ => Err(TimerError::NotFound),
    }
}

/// 原子替换 Process 的 ITIMER_REAL，并返回旧 setting。
pub(crate) fn set_real_timer(
    tgid: usize,
    value_ns: u64,
    interval_ns: u64,
    now_ns: u64,
) -> Result<TimerSetting, TimerError> {
    {
        let graph = super::TASK_MANAGER.graph.lock();
        live_process(&graph, tgid)?;
    }
    loop {
        let needs = super::TASK_MANAGER.timers.lock().real_replacement_needs(
            tgid,
            value_ns != 0 || interval_ns != 0,
            value_ns != 0,
        );
        let prepared =
            PreparedRealReplacement::prepare(tgid, value_ns, interval_ns, now_ns, needs)?;
        let graph = super::TASK_MANAGER.graph.lock();
        live_process(&graph, tgid)?;
        if let Some(previous) = super::TASK_MANAGER
            .timers
            .lock()
            .replace_real(prepared, now_ns)
        {
            return Ok(previous);
        }
    }
}

/// 查询 Process 当前 ITIMER_REAL setting。
pub(crate) fn real_timer(tgid: usize, now_ns: u64) -> Result<TimerSetting, TimerError> {
    let graph = super::TASK_MANAGER.graph.lock();
    live_process(&graph, tgid)?;
    Ok(super::TASK_MANAGER.timers.lock().real(tgid, now_ns))
}

/// 在 live Process 内创建未 armed 的 POSIX timer。
pub(crate) fn create_posix_timer(
    tgid: usize,
    clock: PosixTimerClock,
    notification: PosixTimerNotification,
) -> Result<i32, TimerError> {
    // 初次 lifecycle 检查维持 NotFound 优先；最终 graph→timer 复查是 publication point。
    {
        let graph = super::TASK_MANAGER.graph.lock();
        let threads = live_process(&graph, tgid)?;
        if let PosixTimerNotification::Thread { tid, .. } = notification
            && !threads.contains_key(&tid)
        {
            return Err(TimerError::NotFound);
        }
    }
    let mut id = super::TASK_MANAGER.timers.lock().next_posix_id(tgid)?;
    let mut prepared = PreparedPosixCreate::prepare(tgid, id, clock, notification)?;
    loop {
        let graph = super::TASK_MANAGER.graph.lock();
        let threads = live_process(&graph, tgid)?;
        if let PosixTimerNotification::Thread { tid, .. } = notification
            && !threads.contains_key(&tid)
        {
            return Err(TimerError::NotFound);
        }
        let commit = super::TASK_MANAGER
            .timers
            .lock()
            .commit_posix_create(prepared);
        match commit {
            Ok(()) => return Ok(id),
            Err(collision) => {
                drop(graph);
                id = super::TASK_MANAGER.timers.lock().next_posix_id(tgid)?;
                prepared = collision.retarget(id);
            }
        }
    }
}

/// 原子替换一个 Process-owned POSIX timer，并返回旧 setting。
pub(crate) fn set_posix_timer(
    tgid: usize,
    id: i32,
    value_ns: u64,
    interval_ns: u64,
    absolute: bool,
    now_ns: u64,
) -> Result<TimerSetting, TimerError> {
    {
        let graph = super::TASK_MANAGER.graph.lock();
        live_process(&graph, tgid)?;
        super::TASK_MANAGER.timers.lock().posix(tgid, id, now_ns)?;
    }
    loop {
        let deadline_needed =
            super::TASK_MANAGER
                .timers
                .lock()
                .posix_deadline_needed(tgid, id, value_ns != 0)?;
        let prepared = PreparedPosixReplacement::prepare(
            tgid,
            id,
            value_ns,
            interval_ns,
            absolute,
            deadline_needed,
        )?;
        let graph = super::TASK_MANAGER.graph.lock();
        live_process(&graph, tgid)?;
        if let Some(previous) = super::TASK_MANAGER
            .timers
            .lock()
            .replace_posix(prepared, now_ns)?
        {
            return Ok(previous);
        }
    }
}

/// 查询一个 Process-owned POSIX timer。
pub(crate) fn posix_timer(tgid: usize, id: i32, now_ns: u64) -> Result<TimerSetting, TimerError> {
    let graph = super::TASK_MANAGER.graph.lock();
    live_process(&graph, tgid)?;
    super::TASK_MANAGER.timers.lock().posix(tgid, id, now_ns)
}

/// 查询一个 Process-owned POSIX timer 的最近 overrun。
pub(crate) fn posix_timer_overrun(tgid: usize, id: i32) -> Result<i32, TimerError> {
    let graph = super::TASK_MANAGER.graph.lock();
    live_process(&graph, tgid)?;
    super::TASK_MANAGER.timers.lock().posix_overrun(tgid, id)
}

/// 删除一个 Process-owned POSIX timer。
pub(crate) fn delete_posix_timer(tgid: usize, id: i32) -> Result<(), TimerError> {
    let graph = super::TASK_MANAGER.graph.lock();
    live_process(&graph, tgid)?;
    super::TASK_MANAGER.timers.lock().delete_posix(tgid, id)
}

/// 在 exec point-of-no-return 删除 Linux 不继承的全部 POSIX timers。
pub(crate) fn remove_posix_timers_for_exec(tgid: usize) {
    let graph = super::TASK_MANAGER.graph.lock();
    live_process(&graph, tgid).expect("exec process missing from timer lifecycle");
    super::TASK_MANAGER.timers.lock().remove_posix_timers(tgid);
}
