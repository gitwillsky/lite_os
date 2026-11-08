//! 调度分区：一个授权 worker 独占一个逻辑 CPU，其余线程留在共享核。
//! 配置锁只保护冷事务；分派以每 CPU owner 的原子投影过滤，旧队列条目按原 generation 迁移。
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

/// 无可用分区与执行体退出是不同错误；失败不改变已提交配置。
pub(crate) enum IsolationError {
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

/// 预检普通线程仍有 active CPU；不能通过隔离偷偷放宽其他线程的硬 affinity。
fn admits_partition(threads: &[Arc<Thread>], owner: u64, shared: CpuSet) -> bool {
    threads.iter().all(|thread| {
        let state = thread.scheduling.state.lock();
        thread.id() == owner
            || state.run_state() == RunState::Exited
            || state.cpu_affinity.is_exclusive()
            || !state.cpu_affinity.eligible_in(shared).is_empty()
    })
}

/// 替换 worker 的独占核；None 归还独占核并恢复全部 possible CPU 的普通 affinity。
///
/// # Errors
/// 退出返回 Terminated；非 active CPU、占用冲突、无 housekeeping 核、硬 affinity/CBS 冲突
/// 返回 Conflict；快照分配失败返回 NoMemory。接纳后等待目标 CPU 的上下文保存安全点，
/// 锁外 yield 推进迁移，不忙等远端。
pub(in crate::thread) fn replace_isolation(
    thread: &Arc<Thread>,
    target: Option<CpuId>,
) -> Result<(), IsolationError> {
    let topology = PROCESSOR_TOPOLOGY.wait();
    // 1. 锁外准备快照容量；索引可能并发增长，取得配置锁后复核再复制。
    // snapshot 必须先于 configuration 声明，错误返回也先解锁再析构强引用。
    // 否则恰好退出的线程可能在持配置锁时释放栈/地址空间并等待远端 TLB。
    let mut threads = Vec::new();
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
    // 2. 在唯一配置事务中预检并发表；新创建的执行体使用全体共享核，不能破坏本次预检。
    let old = topology
        .slots
        .iter()
        .find(|slot| slot.processor.exclusive_owner.load(Ordering::Acquire) == thread.id())
        .map(|slot| slot.cpu_id);
    if let Some(cpu) = target {
        if !cpu::is_active(cpu) {
            return Err(IsolationError::Conflict);
        }
        let owner = processor_at(cpu.index())
            .exclusive_owner
            .load(Ordering::Acquire);
        if owner != 0 && owner != thread.id() {
            return Err(IsolationError::Conflict);
        }
        let mut shared = shared_cpus() & cpu::active();
        if let Some(old) = old {
            shared.insert(old);
        }
        shared.remove(cpu);
        if shared.is_empty() || !admits_partition(&threads, thread.id(), shared) {
            return Err(IsolationError::Conflict);
        }
        // 截止预算只能在其原准入分区执行；先撤销旧预算，再改变独占核。
        // 退出可能并发归还预算，读取旧非零值最多保守拒绝，不会超售。
        if old != target && topology.deadline_bandwidth.lock()[cpu.index()] != 0 {
            return Err(IsolationError::Conflict);
        }
    }
    let epoch;
    let (ready, running) = {
        let mut state = thread.scheduling.state.lock();
        if state.run_state() == RunState::Exited || thread.is_killed() {
            return Err(IsolationError::Terminated);
        }
        if old != target
            && target.is_some()
            && thread.scheduling.policy.lock().deadline_cpu().is_some()
        {
            return Err(IsolationError::Conflict);
        }
        state.cpu_affinity = target.map_or_else(CpuAffinity::all, CpuAffinity::exclusive);
        if let Some(cpu) = old.filter(|old| Some(*old) != target) {
            processor_at(cpu.index())
                .exclusive_owner
                .store(0, Ordering::Release);
        }
        if let Some(cpu) = target {
            processor_at(cpu.index())
                .exclusive_owner
                .store(thread.id(), Ordering::Release);
        }
        let mut shared = shared_cpus();
        if let Some(cpu) = old {
            shared.insert(cpu);
        }
        if let Some(cpu) = target {
            shared.remove(cpu);
        }
        topology
            .shared_cpus
            .store(shared.native_word(), Ordering::Release);
        // 新 epoch 必须晚于共享集合发表；旧安全点的确认不能覆盖新分区的迁移。
        epoch = target.map(|cpu| {
            let slot = processor_at(cpu.index());
            if old != target {
                let epoch = crate::id::next_koid();
                slot.exclusive_epoch.store(epoch, Ordering::Release);
                epoch
            } else {
                slot.exclusive_epoch.load(Ordering::Acquire)
            }
        });
        relocate_after_affinity(thread, &mut state)
    };
    drop(configuration);
    drop(threads);
    // 3. 全部锁和快照引用都已释放，才投递迁移、等待远端安全点。
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
    if let Some(cpu) = target {
        let slot = processor_at(cpu.index());
        publish_reschedule_at(cpu);
        loop {
            // 若调用者就是目标 worker，当前上下文已属于 owner，可以直接确认。
            let irq = LocalIrqGuard::disable();
            observe_partition();
            drop(irq);
            if slot.exclusive_owner.load(Ordering::Acquire) != thread.id()
                || Some(slot.exclusive_epoch.load(Ordering::Acquire)) != epoch
            {
                return Err(IsolationError::Terminated);
            }
            if Some(slot.exclusive_ack.load(Ordering::Acquire)) == epoch {
                break;
            }
            super::super::yield_current();
        }
    }
    Ok(())
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
