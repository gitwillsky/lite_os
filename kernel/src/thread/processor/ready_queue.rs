use super::*;

/// 将已发布 Ready generation 的 entry 加入 owner CPU runqueue。
///
/// # Parameters
///
/// - `processor`: 当前 CPU 独占的 scheduler 执行状态。
/// - `entry`: 已属于 processor CPU 的 membership token。
pub(super) fn add_ready_entry(processor: &mut Processor, entry: RunQueueEntry) {
    if !processor.runqueue.has_room(1) {
        let mut spare = core::mem::take(&mut current_per_cpu().inbound.lock().spare_runqueue);
        make_runqueue_room(processor, 1, &mut spare);
    }
    processor.runqueue.push(entry);
    // Membership/floor 与抢占是不同事实；delivery 原因决定能否请求抢占。
    update_floor(processor);
}

/// 以前进的 current/有效 Ready root 更新本 CPU 的虚拟时间基准；idle 不重置时钟。
pub(super) fn update_floor(processor: &mut Processor) {
    discard_stale_ready_roots(processor);
    let current_vruntime = processor
        .current
        .as_ref()
        .map(|current| current.scheduling.policy.lock().vruntime);
    let floor = match (current_vruntime, processor.runqueue.minimum_vruntime()) {
        (Some(current), Some(ready)) => Some(current.min(ready)),
        (current, ready) => current.or(ready),
    };
    if let Some(floor) = floor {
        current_per_cpu().advance_placement_floor(floor);
    }
}

#[cold]
#[inline(never)]
fn make_runqueue_room(processor: &mut Processor, additional: usize, spare: &mut CfsRunQueue) {
    processor.runqueue.adopt_spare(spare);
    let cpu = processor.cpu_id;
    processor
        .runqueue
        .make_room(additional, |candidate| candidate.is_current_ready(cpu));
}

#[inline(always)]
fn discard_stale_ready_roots(processor: &mut Processor) {
    let cpu = processor.cpu_id;
    processor
        .runqueue
        .discard_stale_roots(|candidate| candidate.is_current_ready(cpu));
}

#[cold]
#[inline(never)]
fn compact_full_mailbox(inbound: &mut VecDeque<RunQueueEntry>, cpu_id: CpuId, capacity: usize) {
    assert_eq!(
        inbound.len(),
        capacity,
        "mailbox compaction requires capacity pressure"
    );
    inbound.retain(|candidate| candidate.is_current_ready(cpu_id));
}

/// 消费 stale entry，完成唯一 Ready → Running membership 转换。
///
/// # Parameters
///
/// - `processor`: 当前 CPU 独占的 scheduler 执行状态。
///
/// # Returns
///
/// 队列为空时返回 `None`，否则返回唯一取出的任务。
pub(super) fn select(processor: &mut Processor) -> Option<Arc<Thread>> {
    assert!(
        processor.current.is_none(),
        "CPU already owns a current task"
    );
    let slot = current_per_cpu();
    loop {
        let entry = processor.runqueue.pop()?;
        let mut scheduling = entry.thread.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::Ready { cpu, generation }
                if cpu == processor.cpu_id && generation == entry.generation =>
            {
                // 旧 current 的请求已由本次选择兑现；state guard 阻止新 Running owner 的请求被清掉。
                slot.reschedule_requested.store(false, Ordering::Relaxed);
                commit_ready_retirement(
                    scheduling.transition_ready_to_running(processor.cpu_id, entry.generation),
                );
                drop(scheduling);
                processor.current = Some(entry.thread.clone());
                update_floor(processor);
                slot.running_entries.fetch_add(1, Ordering::Relaxed);
                return Some(entry.thread);
            }
            _ => {
                // generation 不匹配说明后续转换已废弃该 entry，只消费不执行。
            }
        }
    }
}

/// 在一次 mailbox lock 内批量转移本轮 inbound snapshot。
///
/// # Parameters
///
/// - `processor`: 当前 CPU 独占的 scheduler 执行状态。
///
/// # Returns
/// 本次接管的有效 entry 是否早于已计费 current；空/过期 batch 返回 false。
pub(super) fn drain_inbound(processor: &mut Processor) -> bool {
    let slot = current_per_cpu();
    // 锁内只消费进入本轮时的 snapshot；后续 delivery 等待锁，
    // 解锁后留给下一轮。
    let mut inbound = slot.inbound.lock();
    if inbound.entries.is_empty() {
        return false;
    }
    let current = processor.current.as_ref().map(|thread| {
        thread
            .scheduling
            .policy
            .lock()
            .checkpoint_for_wakeup(crate::execution::timer::get_time_us())
    });
    let mut preempt = false;
    inbound.entries.retain(|candidate| {
        let live = candidate.is_current_ready(processor.cpu_id);
        if live {
            preempt |= crate::thread::scheduler::preemption_policy::local_ready_preempts(
                current,
                Some(candidate.vruntime),
            );
        }
        live
    });
    if inbound.entries.is_empty() {
        return false;
    }

    // inbound 每个 entry 都在这一次全量 pass 中验证；合并后只可能按 heap root
    // 可见性复查连续 root。只有容量不足时才采用已准备的存储或清理过期 entry。
    let batch = inbound.entries.len();
    if !processor.runqueue.has_room(batch) {
        make_runqueue_room(processor, batch, &mut inbound.spare_runqueue);
    }
    for _ in 0..batch {
        processor.runqueue.push(
            inbound
                .entries
                .pop_front()
                .expect("inbound batch shrank without mailbox owner"),
        );
    }
    update_floor(processor);
    preempt
}

/// 投递 Ready entry；本地只为真实新唤醒裁决，远端由目标 CPU 接管 mailbox。
///
/// # Parameters
///
/// - `cpu_id`: 目标 logical CPU identity。
/// - `cause`: 新唤醒可裁决抢占；本地 SwitchOut 只回队，让已经选定的 successor 先继续执行。
/// - `entry`: 带 generation 的 membership token。
///
/// # Errors
///
/// 目标越界、未 active 或 platform IPI 失败均 fail-stop。
#[inline(always)]
pub(super) fn deliver_ready_entry(cpu_id: CpuId, entry: RunQueueEntry, cause: ReadyCause) {
    if cpu_id == cpu::current_id() {
        deliver_local(entry, cause);
    } else {
        deliver_remote(cpu_id, entry);
    }
}

#[inline(never)]
fn deliver_local(entry: RunQueueEntry, cause: ReadyCause) {
    let preempt = with_current_processor(|processor| {
        let preempt = matches!(cause, ReadyCause::Wake)
            && entry.is_current_ready(processor.cpu_id)
            && crate::thread::scheduler::preemption_policy::local_ready_preempts(
                processor.current.as_ref().map(|thread| {
                    thread
                        .scheduling
                        .policy
                        .lock()
                        .checkpoint_for_wakeup(crate::execution::timer::get_time_us())
                }),
                Some(entry.vruntime),
            );
        processor.add_ready_entry(entry);
        preempt
    });
    if preempt {
        request_reschedule();
    }
}

#[inline(never)]
fn deliver_remote(cpu_id: CpuId, entry: RunQueueEntry) {
    let target = processor_at(cpu_id.index());
    assert!(cpu::is_active(cpu_id));
    let mut inbound = target.inbound.lock();
    if inbound.entries.len() == inbound.entries.capacity() {
        if inbound.spare_entries.capacity() > inbound.entries.capacity() {
            let mut spare = core::mem::take(&mut inbound.spare_entries);
            spare.append(&mut inbound.entries);
            inbound.entries = spare;
        } else {
            let capacity = inbound.entries.capacity();
            compact_full_mailbox(&mut inbound.entries, cpu_id, capacity);
        }
    }
    assert!(
        inbound.entries.len() < inbound.entries.capacity(),
        "preallocated scheduler mailbox capacity exhausted"
    );
    inbound.entries.push_back(entry);
    drop(inbound);
    // Sender 不读取远端 current/runqueue，也不强置 reschedule。目标在安全点接管 mailbox，
    // 使用自己已计费的 current 与 Ready root 裁决；缺失时跨服务 wake 可被拖到下一 10 ms tick。
    crate::execution::deferred::raise_on(
        cpu_id,
        crate::execution::deferred::DeferredWork::RUNQUEUE,
    );
}

impl RunQueueEntry {
    /// 核对 entry generation 与唯一 SchedulingState Ready membership。
    ///
    /// # Parameters
    ///
    /// - `cpu`: 容器所属 CPU ID。
    ///
    /// # Returns
    ///
    /// 该 entry 仍是当前唯一 Ready membership 时返回 true。
    fn is_current_ready(&self, cpu: CpuId) -> bool {
        matches!(
            self.thread.scheduling.state.lock().run_state(),
            RunState::Ready {
                cpu: owner,
                generation
            } if owner == cpu && generation == self.generation
        )
    }
}
