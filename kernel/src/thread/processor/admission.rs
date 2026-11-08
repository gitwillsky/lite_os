//! 调度接纳：线程容量在创建时准备；分区 EDF 带宽按唯一凭据在短账本事务内扣旧添新。
//! 睡眠不释放带宽承诺，退出/撤销才归还，保留公平类余量。
use super::*;
use crate::thread::scheduler::deadline::Parameters;

/// 一个存活线程的调度容量凭据；即使所有就绪线程集中在同一 CPU，队列仍能容纳。
pub(in crate::thread) struct ThreadSlot(());

impl ThreadSlot {
    /// 在创建线程的可失败阶段，先确保每个 CPU 的 mailbox 与有序集合都能容纳所有已接纳线程。
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
        spares.push((entries, RunQueue::try_with_capacity(capacity)?));
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

const DEADLINE_CAPACITY_Q32: u64 = ((1u64 << 32) * 95) / 100;

/// 准入容量与执行体生命周期是不同错误，不能让已退出线程表现为带宽不足。
pub(crate) enum DeadlineError {
    Capacity,
    Terminated,
}

/// 一个 CPU 上的唯一带宽凭据；不能复制，失去最终 owner 自动归还。
pub(in crate::thread) struct DeadlineReservation {
    cpu: CpuId,
    density: u64,
}

impl DeadlineReservation {
    /// 选择密度最低且亲和允许的 active CPU；失败不改变旧承诺或任何账本。
    pub(in crate::thread) fn replace(
        previous: Option<&mut Self>,
        parameters: Parameters,
        affinity: CpuAffinity,
    ) -> Result<Self, ()> {
        let mut ledger = PROCESSOR_TOPOLOGY.wait().deadline_bandwidth.lock();
        let density = parameters.density();
        let candidate = cpu::active()
            .iter()
            .filter(|&cpu| affinity.allows(cpu))
            .filter_map(|cpu| {
                let refund = previous
                    .as_ref()
                    .filter(|old| old.cpu == cpu)
                    .map_or(0, |old| old.density);
                let used = ledger[cpu.index()] - refund;
                (used.saturating_add(density) <= DEADLINE_CAPACITY_Q32).then_some((used, cpu))
            })
            .min_by_key(|&(used, cpu)| (used, cpu.index()))
            .ok_or(())?;
        if let Some(old) = previous {
            ledger[old.cpu.index()] -= old.density;
            old.density = 0;
        }
        ledger[candidate.1.index()] += density;
        Ok(Self {
            cpu: candidate.1,
            density,
        })
    }

    pub(in crate::thread) fn cpu(&self) -> CpuId {
        self.cpu
    }
}

impl Drop for DeadlineReservation {
    fn drop(&mut self) {
        if self.density != 0 {
            let mut ledger = PROCESSOR_TOPOLOGY.wait().deadline_bandwidth.lock();
            ledger[self.cpu.index()] = ledger[self.cpu.index()]
                .checked_sub(self.density)
                .expect("deadline reservation released twice");
        }
    }
}
