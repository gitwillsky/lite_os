//! POSIX timer 的 signal generation 与忽略通知恢复事务。
use super::*;

/// POSIX expiry 只移动创建时的实际预留 record；不走动态 RT preparation 或 kill fallback。
/// graph → timer 复查后释放 timer guard，才进入 disposition/pending owner。
pub(in crate::task::process_table) fn publish_posix_timer_signal(
    expired: super::timer_queue::ExpiredPosixTimer,
) {
    use super::timer_queue::PosixTimerNotification;
    let (signal, value, directed_tid) = match expired.notification {
        PosixTimerNotification::Process { signal, value } => (signal, value, None),
        PosixTimerNotification::Thread { tid, signal, value } => (signal, value, Some(tid)),
        PosixTimerNotification::Default | PosixTimerNotification::None => {
            unreachable!("normalized notification")
        }
    };
    let mut retired = crate::task::model::RetiredSignals::default();
    let publication = {
        let (mut child_records, mut graph) = if signal == syscall_abi::signal::SIGCONT {
            let Some((records, graph)) =
                lock_child_notifications(expired.tgid, ChildNotificationScope::Parent)
            else {
                return;
            };
            (Some(records), graph)
        } else {
            (None, PROCESS_TABLE.graph.lock())
        };
        let Some(ProcessState::Live(threads)) =
            graph.nodes.get(&expired.tgid).map(|node| &node.state)
        else {
            return;
        };
        let target = match directed_tid {
            Some(tid) => threads.get(&tid).filter(|target| {
                expired
                    .thread_target
                    .as_ref()
                    .is_some_and(|expected| Arc::ptr_eq(expected, &target.tid_identity()))
            }),
            None => threads.values().next(),
        }
        .cloned();
        let Some(target) = target else { return };
        let Some(mut prepared) = PROCESS_TABLE
            .timers
            .lock()
            .prepare_posix_notification(&expired)
        else {
            return;
        };
        // 同一 timer 的旧 record 尚在 pending：只更新其有效 sequence，不增加第二个事件。
        if !prepared.has_record() {
            return;
        }
        let info = PendingSignal::timer(expired.id, 0, value);
        let (queued, discarded, default_fatal) = match directed_tid {
            Some(_) => target.queue_signal(
                threads.values(),
                signal,
                super::ptrace::thread_traced(&graph, target.tid()),
                info,
                &mut prepared,
            ),
            None => target.queue_leader_process_signal(
                threads.values(),
                signal,
                super::ptrace::group_traced(&graph, target.tgid()),
                info,
                &mut prepared,
            ),
        }
        .expect("preallocated timer record is present");
        retired.append(discarded);
        retired
            .recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
        if !queued {
            PROCESS_TABLE.timers.lock().ignore_posix_expiry(&expired);
            // graph 保活实际 timer registry Arc；这里只归还 Box，不收费/析构最后 owner。
            prepared.recycle_timer();
        }
        let notification = if signal == syscall_abi::signal::SIGCONT {
            continue_process_locked(
                &mut graph,
                expired.tgid,
                child_records.as_mut().expect("SIGCONT child preparation"),
            )
        } else {
            if signal == syscall_abi::signal::SIGKILL {
                resume_for_fatal_signal_locked(&mut graph, expired.tgid);
            }
            None
        };
        let group_exit_started = queued
            && if directed_tid.is_some() {
                complete_fatal_signal_locked(&mut graph, &target, signal, default_fatal)
            } else {
                let eligible = graph
                    .nodes
                    .get(&expired.tgid)
                    .and_then(|node| match &node.state {
                        ProcessState::Live(threads) => threads
                            .values()
                            .find(|thread| {
                                thread.accepts_process_signal(
                                    signal,
                                    super::ptrace::thread_traced(&graph, thread.tid()),
                                )
                            })
                            .cloned(),
                        _ => None,
                    });
                eligible.is_some_and(|target| {
                    complete_fatal_signal_locked(&mut graph, &target, signal, default_fatal)
                })
            };
        // prepared 已全部转移或归还；其 Drop 不触碰 allocator/rlimit。
        (target, queued, notification, group_exit_started)
    };
    drop(retired);
    let (target, queued, notification, group_exit_started) = publication;
    publish_job_notification(notification);
    if group_exit_started {
        super::process_exit::wake_exiting_group(expired.tgid, None);
        return;
    }
    if !queued {
        return;
    }
    let woke = if directed_tid.is_some() {
        notify_signal_waiters(&target);
        wake_signal_waiter(&target)
    } else {
        wake_process_signal_waiter(expired.tgid)
    };
    if !woke && !interrupt_waiting_task(&target) {
        crate::task::processor::request_task_reschedule(&target);
    }
}

pub(super) fn resume_ignored_posix_timers(tgid: usize, signal: usize) {
    let mut cursor = -1;
    loop {
        let expired = {
            let graph = PROCESS_TABLE.graph.lock();
            if !matches!(
                graph.nodes.get(&tgid).map(|node| &node.state),
                Some(ProcessState::Live(_))
            ) {
                return;
            }
            PROCESS_TABLE
                .timers
                .lock()
                .resume_ignored_posix(tgid, signal, cursor)
        };
        let Some(expired) = expired else { return };
        cursor = expired.id;
        publish_posix_timer_signal(expired);
    }
}
