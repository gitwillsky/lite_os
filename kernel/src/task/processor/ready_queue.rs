use super::*;

/// @description 将已发布 Ready generation 的 entry 加入 owner hart runqueue。
/// @param processor 当前 hart 独占的 scheduler 执行状态。
/// @param entry 已属于 processor hart 的 membership token。
/// @return 无返回值。
pub(super) fn add_ready_entry(processor: &mut Processor, entry: RunQueueEntry) {
    let slot = current_per_hart();
    discard_stale_ready_entries(processor);
    processor.runqueue.push(entry);
    publish_runqueue_state(processor, slot);
}

fn discard_stale_ready_entries(processor: &mut Processor) {
    processor
        .runqueue
        .retain(|candidate| candidate.is_current_ready(processor.hart_id));
}

fn publish_runqueue_state(processor: &Processor, slot: &PerHartProcessor) {
    if let Some(floor) = processor.runqueue.minimum_vruntime() {
        slot.placement_vruntime.store(floor, Ordering::Release);
    }
    // local heap 只有 owner hart 修改；单次发布容器精确长度，不统计 mailbox/current。
    slot.queued_entries
        .store(processor.runqueue.len(), Ordering::Relaxed);
    debug_assert_eq!(
        processor.runqueue.len(),
        slot.queued_entries.load(Ordering::Relaxed)
    );
}

/// @description 消费 stale entry，完成唯一 Ready → Running membership 转换。
/// @param processor 当前 hart 独占的 scheduler 执行状态。
/// @return 队列为空时返回 `None`，否则返回唯一取出的任务。
pub(super) fn select_task(processor: &mut Processor) -> Option<Arc<TaskControlBlock>> {
    assert!(
        processor.current.is_none(),
        "CPU already owns a current task"
    );
    let slot = current_per_hart();
    loop {
        let entry = processor.runqueue.pop()?;
        slot.queued_entries.fetch_sub(1, Ordering::Relaxed);
        let mut scheduling = entry.task.scheduling.state.lock();
        match scheduling.run_state {
            RunState::Ready { cpu, generation }
                if cpu == processor.hart_id && generation == entry.generation =>
            {
                scheduling.run_state = RunState::Running {
                    cpu: processor.hart_id,
                };
                drop(scheduling);
                processor.current = Some(entry.task.clone());
                let floor = processor
                    .runqueue
                    .minimum_vruntime()
                    .unwrap_or(entry.vruntime);
                slot.placement_vruntime.store(floor, Ordering::Release);
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
/// @param processor 当前 hart 独占的 scheduler 执行状态。
/// @return 无返回值。
pub(super) fn drain_inbound_to_local(processor: &mut Processor) {
    let slot = current_per_hart();
    // 锁内只消费进入本轮时的 snapshot；后续 delivery 等待锁，
    // 解锁后留给下一轮。VecDeque 不 swap，始终保留启动期预留 backing。
    let mut inbound = slot.inbound.lock();
    if inbound.is_empty() {
        return;
    }
    inbound.retain(|candidate| candidate.is_current_ready(processor.hart_id));
    // 非空 mailbox snapshot 总是与 local heap 组成同一 cleanup transaction；
    // 即使 mailbox 全部 stale，也不应让 local load hint 继续保留失效 entry。
    discard_stale_ready_entries(processor);
    if inbound.is_empty() {
        publish_runqueue_state(processor, slot);
        slot.inbound_entries.store(0, Ordering::Relaxed);
        return;
    }

    // 在 mailbox lock 内完成 local sweep 可以使 Ready migration 的新
    // delivery 等待，从而不会同时保留旧 local generation。
    let batch = inbound.len();
    let required = processor
        .runqueue
        .len()
        .checked_add(batch)
        .expect("scheduler membership count overflow");
    assert!(
        required <= slot.queue_capacity,
        "preallocated scheduler runqueue capacity exhausted"
    );
    for _ in 0..batch {
        processor.runqueue.push(
            inbound
                .pop_front()
                .expect("inbound batch shrank without mailbox owner"),
        );
    }
    publish_runqueue_state(processor, slot);
    slot.inbound_entries.store(0, Ordering::Relaxed);
}

/// @description 投递 Ready entry；busy target 同步 reschedule，避免 writer 饿死 Ready reader。
/// @param cpu_id 目标 raw hart ID。
/// @param entry 带 generation 的 membership token。
/// @return 无返回值。
/// @errors 目标越界、未 active 或 SBI IPI 失败均 fail-stop。
pub(super) fn deliver_ready_entry(cpu_id: usize, entry: RunQueueEntry) {
    let current = hart_id();
    if cpu_id == current {
        with_current_processor(|processor| processor.add_ready_entry(entry));
        if current_per_hart().running_entries.load(Ordering::Relaxed) != 0 {
            request_reschedule();
        }
        return;
    }

    let target_index = hart::hart_index(cpu_id)
        .unwrap_or_else(|| panic!("target CPU {} is absent from DTB topology", cpu_id));
    let target_state = &hart::states()[target_index];
    let target = processor_at(target_index);
    assert!(target_state.is_active());
    let mut inbound = target.inbound.lock();
    inbound.retain(|candidate| candidate.is_current_ready(cpu_id));
    assert!(
        inbound.len() < target.queue_capacity,
        "preallocated scheduler mailbox capacity exhausted"
    );
    inbound.push_back(entry);
    target
        .inbound_entries
        .store(inbound.len(), Ordering::Relaxed);
    drop(inbound);
    publish_reschedule_at(target_index);
}

impl RunQueueEntry {
    /// @description 核对 entry generation 与唯一 SchedulingState Ready membership。
    /// @param cpu 容器所属 hart ID。
    /// @return 该 entry 仍是当前唯一 Ready membership 时返回 true。
    fn is_current_ready(&self, cpu: usize) -> bool {
        matches!(
            self.task.scheduling.state.lock().run_state,
            RunState::Ready {
                cpu: owner,
                generation
            } if owner == cpu && generation == self.generation
        )
    }
}
