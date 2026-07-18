use super::*;

/// @description 将已发布 Ready generation 的 entry 加入 owner CPU runqueue。
/// @param processor 当前 CPU 独占的 scheduler 执行状态。
/// @param entry 已属于 processor CPU 的 membership token。
/// @return 同一 slot 当前是否已有 Running owner。
pub(super) fn add_ready_entry(processor: &mut Processor, entry: RunQueueEntry) -> bool {
    let slot = current_per_cpu();
    make_runqueue_room(processor, 1);
    processor.runqueue.push(entry);
    discard_stale_ready_roots(processor);
    publish_vruntime_floor(processor, slot);
    slot.running_entries.load(Ordering::Relaxed) != 0
}

#[inline(always)]
fn make_runqueue_room(processor: &mut Processor, additional: usize) {
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

fn publish_vruntime_floor(processor: &Processor, slot: &PerCpuProcessor) {
    if let Some(floor) = processor.runqueue.minimum_vruntime() {
        slot.placement_vruntime.store(floor, Ordering::Release);
    }
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

/// @description 消费 stale entry，完成唯一 Ready → Running membership 转换。
/// @param processor 当前 CPU 独占的 scheduler 执行状态。
/// @return 队列为空时返回 `None`，否则返回唯一取出的任务。
pub(super) fn select_task(processor: &mut Processor) -> Option<Arc<TaskControlBlock>> {
    assert!(
        processor.current.is_none(),
        "CPU already owns a current task"
    );
    let slot = current_per_cpu();
    loop {
        let entry = processor.runqueue.pop()?;
        let mut scheduling = entry.task.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::Ready { cpu, generation }
                if cpu == processor.cpu_id && generation == entry.generation =>
            {
                commit_ready_retirement(
                    scheduling.transition_ready_to_running(processor.cpu_id, entry.generation),
                );
                drop(scheduling);
                processor.current = Some(entry.task.clone());
                discard_stale_ready_roots(processor);
                if let Some(floor) = processor.runqueue.minimum_vruntime() {
                    slot.placement_vruntime.store(floor, Ordering::Release);
                } else {
                    slot.placement_vruntime
                        .store(entry.vruntime, Ordering::Release);
                }
                slot.running_entries.fetch_add(1, Ordering::Relaxed);
                return Some(entry.task);
            }
            _ => {
                // generation 不匹配说明 stop/continue 等转换已废弃该 entry，只消费不执行。
            }
        }
    }
}

/// @description 在一次 mailbox lock 内批量转移本轮 inbound snapshot。
/// @param processor 当前 CPU 独占的 scheduler 执行状态。
/// @return 无返回值。
pub(super) fn drain_inbound_to_local(processor: &mut Processor) {
    let slot = current_per_cpu();
    // 锁内只消费进入本轮时的 snapshot；后续 delivery 等待锁，
    // 解锁后留给下一轮。VecDeque 不 swap，始终保留启动期预留 backing。
    let mut inbound = slot.inbound.lock();
    if inbound.is_empty() {
        return;
    }
    inbound.retain(|candidate| candidate.is_current_ready(processor.cpu_id));
    if inbound.is_empty() {
        return;
    }

    // inbound 每个 entry 都在这一次全量 pass 中验证；合并后只可能按 heap root
    // 可见性复查连续 root，不再扫描 batch。local heap 仅在 batch 将越过已预留
    // capacity 时执行全量 compaction。
    let batch = inbound.len();
    make_runqueue_room(processor, batch);
    for _ in 0..batch {
        processor.runqueue.push(
            inbound
                .pop_front()
                .expect("inbound batch shrank without mailbox owner"),
        );
    }
    discard_stale_ready_roots(processor);
    publish_vruntime_floor(processor, slot);
}

/// @description 投递 Ready entry；busy target 同步 reschedule，避免 writer 饿死 Ready reader。
/// @param cpu_id 目标 logical CPU identity。
/// @param entry 带 generation 的 membership token。
/// @return 无返回值。
/// @errors 目标越界、未 active 或 platform IPI 失败均 fail-stop。
#[inline(always)]
pub(super) fn deliver_ready_entry(cpu_id: CpuId, entry: RunQueueEntry) {
    if cpu_id == cpu::current_id() {
        deliver_local(entry);
    } else {
        deliver_remote(cpu_id, entry);
    }
}

#[inline(never)]
fn deliver_local(entry: RunQueueEntry) {
    if with_current_processor(|processor| processor.add_ready_entry(entry)) {
        request_reschedule();
    }
}

#[inline(never)]
fn deliver_remote(cpu_id: CpuId, entry: RunQueueEntry) {
    let target = processor_at(cpu_id.index());
    assert!(cpu::is_active(cpu_id));
    let mut inbound = target.inbound.lock();
    if inbound.len() == target.queue_capacity {
        compact_full_mailbox(&mut inbound, cpu_id, target.queue_capacity);
    }
    assert!(
        inbound.len() < target.queue_capacity,
        "preallocated scheduler mailbox capacity exhausted"
    );
    inbound.push_back(entry);
    drop(inbound);
    publish_reschedule_at(cpu_id);
}

impl RunQueueEntry {
    /// @description 核对 entry generation 与唯一 SchedulingState Ready membership。
    /// @param cpu 容器所属 CPU ID。
    /// @return 该 entry 仍是当前唯一 Ready membership 时返回 true。
    fn is_current_ready(&self, cpu: CpuId) -> bool {
        matches!(
            self.task.scheduling.state.lock().run_state(),
            RunState::Ready {
                cpu: owner,
                generation
            } if owner == cpu && generation == self.generation
        )
    }
}
