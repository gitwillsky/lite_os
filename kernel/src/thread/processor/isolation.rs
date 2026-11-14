//! 调度分区：一个授权 worker 独占一个逻辑 CPU，其余线程留在共享核。
//! 配置锁只保护冷事务；分派用共享 CPU 集合过滤普通线程，旧队列条目按原 generation 迁移。
use super::{
    PROCESSOR_TOPOLOGY, ReadyCause, Thread, current_per_cpu, deliver_ready_entry, processor_at,
    publish_reschedule_at, ready_entry, relocate_after_affinity, with_current_processor,
};
use crate::execution::{
    cpu::{self, CpuId, CpuSet},
    sync::LocalIrqGuard,
};
use crate::thread::scheduling::{CpuAffinity, RunState};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

/// 区分无效参数、准入失败、内存不足和退出或并发替换。
pub(crate) enum IsolationError {
    InvalidAssignment,
    Conflict,
    Terminated,
    NoMemory,
}

/// 分派热路径只读取目标 CPU 的一个原子值，不扫描拓扑。
pub(in crate::thread) fn cpu_is_shared(cpu: CpuId) -> bool {
    shared_cpus().contains(cpu)
}

/// 普通线程可用的 CPU；线程自己的 requested mask 仍然生效。
pub(in crate::thread) fn shared_cpus() -> CpuSet {
    CpuSet::from_native_word(
        PROCESSOR_TOPOLOGY
            .wait()
            .shared_cpus
            .load(Ordering::Acquire),
    )
}

/// ThreadSlot 已准备索引容量；创建后、发布到调度器前登记，不再分配。
pub(in crate::thread) fn register_thread(thread: &Arc<Thread>) {
    let mut threads = PROCESSOR_TOPOLOGY.wait().threads.lock();
    assert!(
        threads.len() < threads.capacity(),
        "thread index lost admission capacity"
    );
    threads.push((thread.id(), Arc::downgrade(thread)));
}

/// 一次准入所有 `changes`。未列出的线程保留原分区，`None` 恢复共享 affinity。
///
/// # Returns
/// 新目标 CPU 已保存旧执行上下文后返回成功。线程退出仍自动归还独占核。
/// # Errors
/// 重复线程或目标、非 active CPU、硬 affinity/CBS 冲突、执行体退出或内存不足。
/// 准入失败保留原分区；发表后有后续配置或退出时返回 Terminated。
pub(crate) fn replace_isolation(
    changes: &[(Arc<Thread>, Option<CpuId>)],
) -> Result<(), IsolationError> {
    if changes.is_empty() {
        return Ok(());
    }
    let topology = PROCESSOR_TOPOLOGY.wait();
    let mut threads = Vec::new();
    let mut states = Vec::new();
    let mut relocations = Vec::new();
    states
        .try_reserve_exact(changes.len())
        .map_err(|_| IsolationError::NoMemory)?;
    relocations
        .try_reserve_exact(changes.len())
        .map_err(|_| IsolationError::NoMemory)?;
    // 1. 锁外准备强引用快照。先解配置锁再析构快照，避免在配置锁内等待远端 TLB。
    let configuration = loop {
        let required = topology.threads.lock().len();
        threads
            .try_reserve_exact(required)
            .map_err(|_| IsolationError::NoMemory)?;
        let configuration = topology.cpu_configuration.lock();
        let index = topology.threads.lock();
        if threads.capacity() >= index.len() {
            threads.extend(index.iter().filter_map(|(_, thread)| thread.upgrade()));
            drop(index);
            break configuration;
        }
        drop(index);
        drop(configuration);
    };
    let is_changing = |owner| changes.iter().any(|(thread, _)| thread.id() == owner);
    let mut owners = [0; usize::BITS as usize];
    let mut shared = cpu::possible();
    for slot in &topology.slots {
        let owner = slot.processor.exclusive_owner.load(Ordering::Acquire);
        if owner != 0 && !is_changing(owner) {
            owners[slot.cpu_id.index()] = owner;
            shared.remove(slot.cpu_id);
        }
    }
    // 2. 预检完整目标集合，允许同一事务内互换归属。准入前不修改任何调度状态。
    let mut assigned = CpuSet::EMPTY;
    for (i, (thread, target)) in changes.iter().enumerate() {
        if changes[..i]
            .iter()
            .any(|(other, _)| other.id() == thread.id())
        {
            return Err(IsolationError::InvalidAssignment);
        }
        if let Some(cpu) = *target {
            if assigned.contains(cpu) {
                return Err(IsolationError::InvalidAssignment);
            }
            if !cpu::is_active(cpu) || owners[cpu.index()] != 0 {
                return Err(IsolationError::Conflict);
            }
            let previous = processor_at(cpu.index())
                .exclusive_owner
                .load(Ordering::Acquire);
            if previous != thread.id() && topology.deadline_bandwidth.lock()[cpu.index()] != 0 {
                return Err(IsolationError::Conflict);
            }
            assigned.insert(cpu);
            owners[cpu.index()] = thread.id();
            shared.remove(cpu);
        }
    }
    let active_shared = shared & cpu::active();
    if active_shared.is_empty() {
        return Err(IsolationError::Conflict);
    }
    for thread in &threads {
        let state = thread.scheduling.state.lock();
        if !is_changing(thread.id())
            && state.run_state() != RunState::Exited
            && !state.cpu_affinity.is_exclusive()
            && state.cpu_affinity.eligible_in(active_shared).is_empty()
        {
            return Err(IsolationError::Conflict);
        }
    }
    for (thread, target) in changes {
        let state = thread.scheduling.state.lock();
        if state.run_state() == RunState::Exited || thread.is_killed() {
            return Err(IsolationError::Terminated);
        }
        if let Some(cpu) = thread.scheduling.policy.lock().deadline_cpu()
            && !target.map_or_else(|| active_shared.contains(cpu), |target| target == cpu)
        {
            return Err(IsolationError::Conflict);
        }
    }
    // 3. 阻止受影响的线程在共享集合与自身 affinity 之间观察到混合两代。
    // 只有 CPU 配置事务同时取多个 state。外层 IRQ guard 覆盖所有内层 guard 的析构。
    let mut epochs = [0; usize::BITS as usize];
    {
        let _irq = LocalIrqGuard::disable();
        let mut states = states;
        states.extend(
            changes
                .iter()
                .map(|(thread, _)| thread.scheduling.state.lock()),
        );
        topology
            .shared_cpus
            .store(shared.native_word(), Ordering::Release);
        for slot in &topology.slots {
            let index = slot.cpu_id.index();
            let previous = slot.processor.exclusive_owner.load(Ordering::Acquire);
            let owner = owners[index];
            if owner != previous {
                slot.processor
                    .exclusive_owner
                    .store(owner, Ordering::Release);
                if owner != 0 {
                    slot.processor
                        .exclusive_epoch
                        .store(crate::id::next_koid(), Ordering::Release);
                }
            }
            epochs[index] = slot.processor.exclusive_epoch.load(Ordering::Acquire);
        }
        for ((thread, target), state) in changes.iter().zip(&mut states) {
            state.cpu_affinity = target.map_or_else(CpuAffinity::all, CpuAffinity::exclusive);
            relocations.push(relocate_after_affinity(thread, state));
        }
    }
    drop(configuration);
    drop(threads);
    // 4. 锁外投递迁移及远端安全点请求。先通知所有 CPU，再等待确认。
    for ((thread, target), (ready, running)) in changes.iter().zip(relocations) {
        if let Some((cpu, generation)) = ready {
            deliver_ready_entry(
                cpu,
                ready_entry(thread.clone(), generation),
                ReadyCause::Wake,
            );
        }
        if let Some(cpu) = running {
            publish_reschedule_at(cpu);
        }
        if let Some(cpu) = *target {
            publish_reschedule_at(cpu);
        }
    }
    if assigned.is_empty() {
        return Ok(());
    }
    loop {
        // 调用者可以是目标 worker；此时已安全，无需等待切换到自己。
        let irq = LocalIrqGuard::disable();
        observe_partition();
        drop(irq);
        let mut acknowledged = true;
        for (thread, target) in changes {
            let Some(cpu) = *target else { continue };
            let slot = processor_at(cpu.index());
            let epoch = epochs[cpu.index()];
            if slot.exclusive_owner.load(Ordering::Acquire) != thread.id()
                || slot.exclusive_epoch.load(Ordering::Acquire) != epoch
            {
                return Err(IsolationError::Terminated);
            }
            acknowledged &= slot.exclusive_ack.load(Ordering::Acquire) == epoch;
        }
        if acknowledged {
            return Ok(());
        }
        super::super::yield_current();
    }
}

/// 已保存 outgoing context 的 continuation/idle 安全点调用；不能在 select 时提前确认。
pub(in crate::thread) fn observe_partition() {
    let slot = current_per_cpu();
    let epoch = slot.exclusive_epoch.load(Ordering::Acquire);
    let owner = slot.exclusive_owner.load(Ordering::Acquire);
    if owner == 0 {
        return;
    }
    let safe = with_current_processor(|processor| {
        processor.pending_handoff.is_none()
            && processor
                .current
                .as_ref()
                .is_none_or(|thread| thread.id() == owner)
    });
    if safe {
        slot.exclusive_ack.store(epoch, Ordering::Release);
    } else {
        publish_reschedule_at(cpu::current_id());
    }
}

/// 线程退出时归还核；旧 generation/句柄继续保活执行体不能延迟资源释放。
pub(in crate::thread) fn exit_partition(thread: &Thread) {
    let topology = PROCESSOR_TOPOLOGY.wait();
    let _configuration = topology.cpu_configuration.lock();
    let mut state = thread.scheduling.state.lock();
    assert!(
        matches!(state.run_state(), RunState::Running { .. }),
        "only the running thread can exit"
    );
    assert!(
        state.wait.is_none(),
        "running thread retains wait membership"
    );
    state.replace_non_ready_state(RunState::Exited);
    let mut shared = shared_cpus();
    for slot in &topology.slots {
        if slot.processor.exclusive_owner.load(Ordering::Acquire) == thread.id() {
            slot.processor.exclusive_owner.store(0, Ordering::Release);
            shared.insert(slot.cpu_id);
        }
    }
    topology
        .shared_cpus
        .store(shared.native_word(), Ordering::Release);
}
