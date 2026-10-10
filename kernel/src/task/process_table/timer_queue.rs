use alloc::sync::{Arc, Weak};

use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    file::{TimerError, TimerFdBackend, TimerSetting},
};

mod kernel_wait;
mod period;
mod posix_creation;
mod posix_schedule;
pub(in crate::task) use posix_schedule::deliver_posix_signal;
use posix_schedule::{PosixSchedule, PosixTimer};
mod preparation;
mod preparation_policy;
mod retirement;
use retirement::{RetiredTimers, TimerExpirationRetirement};
mod timer_file;
mod transaction;
mod transaction_loop;
pub(crate) use kernel_wait::KernelDeadlineWait;
use kernel_wait::{KernelTimer, KernelTimerExpiration};
use period::next_period;
use preparation::{PreparedPosixCreate, PreparedPosixReplacement, PreparedRealReplacement};
use preparation_policy::{TimerReplacementNeeds, real_replacement_needs};
use timer_file::TimerFile;
pub(crate) use timer_file::{TimerFileClock, create_timer_fd};
use transaction::{TimerTransactionPolicy, execute_timer_transaction};

type DeadlineEntry = VacantEntry<(u64, TimerIdentity), Option<Weak<KernelTimer>>>;

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
    Boottime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TimerIdentity {
    Real(usize),
    Posix(usize, i32),
    File(u64),
    Kernel(u64),
}

/// 锁外 signal delivery 所需的完整 POSIX timer 到期值。
pub(super) struct ExpiredPosixTimer {
    pub(super) tgid: usize,
    pub(super) id: i32,
    pub(super) notification: PosixTimerNotification,
    pub(super) sequence: u64,
    pub(super) owner: Arc<crate::task::model::PreallocatedSignal>,
    pub(super) thread_target: Option<Arc<crate::namespace::PidIdentity>>,
}

pub(super) enum ExpiredTimer {
    Real {
        tgid: usize,
        retired: TimerExpirationRetirement,
    },
    Posix(ExpiredPosixTimer),
    File {
        target: Arc<dyn TimerFdBackend>,
        elapsed: u64,
        generation: u64,
        retired: TimerExpirationRetirement,
    },
    Kernel(KernelTimerExpiration),
    Silent(TimerExpirationRetirement),
}

/// ITIMER_REAL、POSIX timer record 与 active deadline index 的唯一复合状态 owner。
pub(super) struct TimerQueue {
    real_timers: FallibleMap<usize, RealTimer>,
    posix_timers: FallibleMap<(usize, i32), PosixTimer>,
    timer_files: FallibleMap<u64, TimerFile>,
    // OWNER: 仅本类型在同一 timer lock 下同步 record 与 active deadline membership。
    // 缺失统一 index 会恢复每 tick O(processes+timers) 扫描；分离写入口会漏发或重复 signal。
    deadline_index: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
}

impl TimerQueue {
    pub(super) const fn new() -> Self {
        Self {
            real_timers: FallibleMap::new(),
            posix_timers: FallibleMap::new(),
            timer_files: FallibleMap::new(),
            deadline_index: FallibleMap::new(),
        }
    }

    fn take_real(&mut self, tgid: usize, retired: &mut RetiredTimers) {
        let Some(timer) = self.real_timers.take_entry(&tgid) else {
            return;
        };
        if let Some(expiration) = timer.value().next_expiration_ns {
            retired.deadlines.commit_vacant(
                self.deadline_index
                    .take_entry(&(expiration, TimerIdentity::Real(tgid)))
                    .expect("actual real deadline"),
            );
        }
        retired.real.commit_vacant(timer);
    }
    fn take_posix(&mut self, key: (usize, i32), retired: &mut RetiredTimers) -> bool {
        let Some(timer) = self.posix_timers.take_entry(&key) else {
            return false;
        };
        if let PosixSchedule::Armed { expiration } = timer.value().schedule {
            retired.deadlines.commit_vacant(
                self.deadline_index
                    .take_entry(&(expiration, TimerIdentity::Posix(key.0, key.1)))
                    .expect("actual POSIX deadline"),
            );
        }
        retired.posix.commit_vacant(timer);
        true
    }
    /// exit 同锁摘除实际 records/index；调用方移出 graph 后消费所有 storage。
    pub(super) fn remove_process(&mut self, tgid: usize) -> RetiredTimers {
        let mut retired = self.remove_posix_timers(tgid);
        self.take_real(tgid, &mut retired);
        retired
    }
    /// exec 不继承 POSIX timers；只遍历本 Process 范围，ITIMER_REAL 保持实际 lifetime。
    pub(super) fn remove_posix_timers(&mut self, tgid: usize) -> RetiredTimers {
        let mut retired = RetiredTimers::default();
        let mut cursor = (tgid, -1);
        while let Some(key) = self
            .posix_timers
            .successor(&cursor)
            .map(|(&key, _)| key)
            .filter(|key| key.0 == tgid)
        {
            assert!(self.take_posix(key, &mut retired));
            cursor = key;
        }
        retired
    }

    fn replace_real(
        &mut self,
        prepared: &mut PreparedRealReplacement,
        now_ns: u64,
    ) -> Option<(TimerSetting, RetiredTimers)> {
        let tgid = prepared.tgid;
        let replacement = prepared.replacement;
        let next = replacement.and_then(|timer| timer.next_expiration_ns);
        let current = self.real_timers.get(&tgid).copied();
        let needs = real_replacement_needs(
            current.is_some(),
            current.is_some_and(|timer| timer.next_expiration_ns.is_some()),
            replacement.is_some(),
            next.is_some(),
        );
        if needs.record && prepared.timer_node.is_none()
            || needs.deadline && prepared.deadline_node.is_none()
        {
            return None;
        }
        let mut retired = RetiredTimers::default();
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
                    self.real_timers.commit_vacant(
                        prepared
                            .timer_node
                            .take()
                            .expect("new real timer node not prepared"),
                    );
                }
            }
            None => {
                if let Some(entry) = self.real_timers.take_entry(&tgid) {
                    retired.real.commit_vacant(entry);
                }
            }
        }
        if let Some(expiration) = next {
            let entry = if let Some(mut entry) = deadline.take() {
                entry.set_key((expiration, identity));
                entry
            } else {
                let mut entry = prepared
                    .deadline_node
                    .take()
                    .expect("new real deadline node not prepared");
                entry.set_key((expiration, identity));
                entry
            };
            self.deadline_index.commit_vacant(entry);
        }
        if let Some(deadline) = deadline {
            retired.deadlines.commit_vacant(deadline);
        }
        Some((previous, retired))
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

    pub(super) fn delete_posix(
        &mut self,
        tgid: usize,
        id: i32,
    ) -> Result<RetiredTimers, TimerError> {
        let mut retired = RetiredTimers::default();
        if !self.take_posix((tgid, id), &mut retired) {
            return Err(TimerError::NotFound);
        }
        Ok(retired)
    }

    pub(super) fn pop_expired(&mut self, now_ns: u64) -> Option<ExpiredTimer> {
        let (&(expiration, identity), _) = self.deadline_index.first_key_value()?;
        if expiration > now_ns {
            return None;
        }
        let mut deadline = Some(
            self.deadline_index
                .take_entry(&(expiration, identity))
                .expect("selected timer deadline disappeared"),
        );
        match identity {
            TimerIdentity::Real(tgid) => {
                let timer = self
                    .real_timers
                    .get_mut(&tgid)
                    .expect("ITIMER_REAL deadline lost record");
                assert_eq!(timer.next_expiration_ns, Some(expiration));
                timer.next_expiration_ns = next_period(expiration, timer.interval_ns, now_ns).0;
                if let Some(next) = timer.next_expiration_ns {
                    let mut entry = deadline.take().expect("actual expired deadline");
                    entry.set_key((next, identity));
                    self.deadline_index.commit_vacant(entry);
                }
                Some(ExpiredTimer::Real {
                    tgid,
                    retired: TimerExpirationRetirement::from_deadline(deadline),
                })
            }
            TimerIdentity::Posix(tgid, id) => Some(self.expire_posix(
                (tgid, id),
                expiration,
                deadline.take().expect("actual POSIX deadline"),
            )),
            TimerIdentity::File(id) => Some(self.pop_expired_timer_file(
                id,
                expiration,
                deadline.take().expect("actual file deadline"),
                now_ns,
            )),
            TimerIdentity::Kernel(_) => Some(KernelTimerExpiration::from_entry(
                deadline.take().expect("actual kernel deadline"),
            )),
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
    execute_timer_transaction(
        TimerTransactionPolicy::itimer_real(tgid),
        |timers| {
            Ok(timers.real_replacement_needs(
                tgid,
                value_ns != 0 || interval_ns != 0,
                value_ns != 0,
            ))
        },
        |needs, reusable| {
            PreparedRealReplacement::prepare(tgid, value_ns, interval_ns, now_ns, needs, reusable)
        },
        |timers, prepared| Ok(timers.replace_real(prepared, now_ns)),
    )
    .map(|(previous, retired)| {
        drop(retired);
        previous
    })
}

/// 查询 Process 当前 ITIMER_REAL setting。
pub(crate) fn real_timer(tgid: usize, now_ns: u64) -> Result<TimerSetting, TimerError> {
    let graph = super::PROCESS_TABLE.graph.lock();
    live_process(&graph, tgid)?;
    Ok(super::PROCESS_TABLE.timers.lock().real(tgid, now_ns))
}

/// 在 live Process 内创建未 armed 的 POSIX timer。
pub(crate) fn create_posix_timer(
    tgid: usize,
    clock: PosixTimerClock,
    notification: PosixTimerNotification,
) -> Result<i32, TimerError> {
    let creator = crate::task::current_task().ok_or(TimerError::NotFound)?;
    let (notification, target) = match notification {
        PosixTimerNotification::Thread { tid, signal, value } => {
            let internal = creator.resolve_pid(tid).ok_or(TimerError::NotFound)?;
            let graph = super::PROCESS_TABLE.graph.lock();
            let threads = live_process(&graph, tgid)?;
            let target = threads
                .get(&internal)
                .ok_or(TimerError::NotFound)?
                .tid_identity();
            (
                PosixTimerNotification::Thread {
                    tid: internal,
                    signal,
                    value,
                },
                Some(target),
            )
        }
        notification => (notification, None),
    };
    execute_timer_transaction(
        TimerTransactionPolicy::posix_create(tgid, target.clone()),
        |timers| timers.next_posix_id(tgid),
        |id, reusable: Option<PreparedPosixCreate>| {
            Ok(match reusable {
                Some(prepared) => prepared.retarget(id),
                None => {
                    PreparedPosixCreate::prepare(tgid, id, clock, notification, target.clone())?
                }
            })
        },
        |timers, prepared| {
            let id = prepared.key.1;
            Ok(timers.commit_posix_create(prepared).then_some(id))
        },
    )
}

/// 原子替换一个 Process-owned POSIX timer，并返回旧 setting。
pub(crate) fn set_posix_timer(
    tgid: usize,
    id: i32,
    value_ns: u64,
    interval_ns: u64,
    absolute: bool,
    now_ns: u64,
    offsets: crate::namespace::TimeOffsets,
) -> Result<TimerSetting, TimerError> {
    execute_timer_transaction(
        TimerTransactionPolicy::posix_replace(tgid),
        |timers| timers.posix_deadline_needed(tgid, id, value_ns != 0),
        |deadline_needed, reusable| {
            PreparedPosixReplacement::prepare(
                tgid,
                id,
                value_ns,
                interval_ns,
                absolute,
                deadline_needed,
                reusable,
            )
        },
        |timers, prepared| timers.replace_posix(prepared, now_ns, offsets),
    )
    .map(|(previous, retired)| {
        drop(retired);
        previous
    })
}

/// 查询一个 Process-owned POSIX timer。
pub(crate) fn posix_timer(tgid: usize, id: i32, now_ns: u64) -> Result<TimerSetting, TimerError> {
    let graph = super::PROCESS_TABLE.graph.lock();
    live_process(&graph, tgid)?;
    super::PROCESS_TABLE.timers.lock().posix(tgid, id, now_ns)
}

/// 查询一个 Process-owned POSIX timer 的最近 overrun。
pub(crate) fn posix_timer_overrun(tgid: usize, id: i32) -> Result<i32, TimerError> {
    let graph = super::PROCESS_TABLE.graph.lock();
    live_process(&graph, tgid)?;
    super::PROCESS_TABLE.timers.lock().posix_overrun(tgid, id)
}

/// 删除一个 Process-owned POSIX timer。
pub(crate) fn delete_posix_timer(tgid: usize, id: i32) -> Result<(), TimerError> {
    let retired = {
        let graph = super::PROCESS_TABLE.graph.lock();
        live_process(&graph, tgid)?;
        super::PROCESS_TABLE.timers.lock().delete_posix(tgid, id)?
    };
    drop(retired);
    Ok(())
}

/// 在 exec point-of-no-return 删除 Linux 不继承的全部 POSIX timers。
pub(crate) fn remove_posix_timers_for_exec(tgid: usize) {
    let retired = {
        let graph = super::PROCESS_TABLE.graph.lock();
        let threads =
            live_process(&graph, tgid).expect("exec process missing from timer lifecycle");
        let timers = super::PROCESS_TABLE.timers.lock().remove_posix_timers(tgid);
        let signals = threads
            .values()
            .next()
            .expect("exec requires survivor")
            .discard_timer_signals(threads.values());
        (timers, signals)
    };
    drop(retired);
}
