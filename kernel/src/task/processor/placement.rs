use super::*;

/// @description 在 active CPU 中选择近似负载最低者。
///
/// @param task 只读取 last-CPU hint，不改变其状态。
/// @param affinity 调用方从同一 SchedulingState transaction 取得的 CPU 集合。
/// @return 被选中的 CPU ID。
pub(super) fn select_cpu(task: &TaskControlBlock, affinity: CpuAffinity) -> CpuId {
    // Relaxed 只用于分散扫描起点，不承担任何状态发布。
    let start = NEXT_CPU.fetch_add(1, Ordering::Relaxed) % cpu::count();
    let current = cpu::current_id();
    // last_cpu 仅提供缓存亲和性提示；过期值只影响候选顺序，不影响任务所有权或可见性。
    let last = cpu::id_at(task.scheduling.last_cpu.load(Ordering::Relaxed));
    let mut best_cpu = None;
    let mut best_load = usize::MAX;
    let mut last_load = None;

    for offset in 0..cpu::count() {
        let cpu_index = (start + offset) % cpu::count();
        let cpu_id = cpu::id_at(cpu_index).expect("logical CPU disappeared from topology");
        if !cpu::is_active(cpu_id) || !affinity.allows(cpu_id) {
            continue;
        }
        let slot = processor_at(cpu_index);
        let load = slot
            .ready_entries
            .load(Ordering::Relaxed)
            .saturating_add(slot.running_entries.load(Ordering::Relaxed));
        if load < best_load {
            best_load = load;
            best_cpu = Some(cpu_id);
        }
        if Some(cpu_id) == last {
            last_load = Some(load);
        }
    }

    match last_load {
        // 仅在同为最小负载时保留缓存亲和性；允许多一个 runnable 会把两个 CPU-bound task 永久压在同一 CPU。
        Some(load) if load == best_load => last.expect("last CPU load requires an identity"),
        _ => best_cpu.unwrap_or_else(|| {
            // init 在 boot CPU 发布 active 前已进入本地 runqueue；缺少这一条装配路径会让
            // 首个 New task 因 active set 暂空而 fail-stop。userspace mask 已另行与 active 相交。
            if affinity.allows(current) {
                current
            } else {
                panic!(
                    "affinity mask {:#x} contains no active CPU",
                    affinity.effective_bits()
                )
            }
        }),
    }
}

pub(super) fn ready_entry(task: Arc<TaskControlBlock>, generation: u64) -> RunQueueEntry {
    let vruntime = task.scheduling.policy.lock().vruntime;
    RunQueueEntry {
        task,
        generation,
        vruntime,
    }
}

fn new_task_placement_floor(cpu: CpuId) -> u64 {
    let slot = processor_at(cpu.index());
    let mut floor = slot.placement_vruntime.load(Ordering::Acquire);
    let inbound = slot.inbound.lock();
    for entry in inbound.iter() {
        if floor == 0 || entry.vruntime < floor {
            floor = entry.vruntime;
        }
    }
    floor
}

/// @description 将新建 Task 从 New 转换为唯一 Ready membership，并按目标 CPU 的
/// Ready/inbound vruntime floor 完成公平 placement。
///
/// @param task process graph 已拥有的初始 Task。
/// @return 选中的 CPU。
pub(crate) fn enqueue_new_task(task: Arc<TaskControlBlock>) -> CpuId {
    let mut scheduling = task.scheduling.state.lock();
    assert_eq!(
        scheduling.run_state(),
        RunState::New,
        "task must start in New"
    );
    let cpu = select_cpu(&task, scheduling.cpu_affinity);
    let floor = new_task_placement_floor(cpu);
    if floor != 0 {
        let mut policy = task.scheduling.policy.lock();
        policy.vruntime = policy.vruntime.max(floor);
    }
    let generation = commit_ready_transition(scheduling.transition_to_ready(cpu));
    drop(scheduling);
    deliver_ready_entry(cpu, ready_entry(task, generation));
    cpu
}
