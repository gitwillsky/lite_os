use super::*;

/// 一个存活线程的调度容量凭据；即使所有就绪线程集中在同一 CPU，队列仍能容纳。
pub(in crate::thread) struct ThreadSlot(());

impl ThreadSlot {
    /// 在创建线程的可失败阶段，先确保每个 CPU 的两条队列都能容纳所有已接纳线程。
    /// # Returns
    /// 成功返回随线程保活的唯一容量凭据。
    /// # Errors
    /// 备用存储分配失败时撤销本次凭据，不发布半套容量。
    pub(in crate::thread) fn try_new() -> Result<Self, ()> {
        let topology = PROCESSOR_TOPOLOGY.wait();
        let previous = topology
            .thread_slots
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1)
            })
            .map_err(|_| ())?;
        let slot = Self(());
        let required = previous + 1;
        if required > *topology.admitted_capacity.lock() {
            grow(topology, required)?;
        }
        Ok(slot)
    }
}

impl Drop for ThreadSlot {
    fn drop(&mut self) {
        let previous = PROCESSOR_TOPOLOGY
            .wait()
            .thread_slots
            .fetch_sub(1, Ordering::Relaxed);
        assert_ne!(previous, 0, "scheduler thread slot count underflow");
    }
}

#[cold]
fn grow(topology: &ProcessorTopology, required: usize) -> Result<(), ()> {
    // 1. 成倍准备所有备用队列；锁外分配，任何失败都保留现有容量。
    let capacity = required.checked_next_power_of_two().ok_or(())?;
    let mut spares = Vec::new();
    spares
        .try_reserve_exact(topology.slots.len())
        .map_err(|_| ())?;
    for _ in &topology.slots {
        let mut entries = VecDeque::new();
        entries.try_reserve_exact(capacity).map_err(|_| ())?;
        spares.push((entries, CfsRunQueue::try_with_capacity(capacity)?));
    }

    // 2. 以 admission → inbound 的锁序发布整批；并发创建可能已发布更大的一批。
    let mut admitted = topology.admitted_capacity.lock();
    if *admitted < capacity {
        for (slot, (entries, runqueue)) in topology.slots.iter().zip(&mut spares) {
            let mut inbound = slot.processor.inbound.lock();
            core::mem::swap(&mut inbound.spare_entries, entries);
            core::mem::swap(&mut inbound.spare_runqueue, runqueue);
        }
        *admitted = capacity;
    }
    // 3. 未使用或被替换的空队列在两个 guard 都释放后归还分配器。
    drop(admitted);
    Ok(())
}
