use super::*;
use crate::execution::task::scheduler::placement_policy;

/// 在 active CPU 中选择近似负载最低者；负载并列时保留 last-CPU 亲和。
///
/// # Parameters
///
/// - `task`: 只读取 last-CPU hint，不改变其状态。
/// - `affinity`: 调用方从同一 SchedulingState transaction 取得的 CPU 集合。
///
/// # Returns
///
/// 被选中的 CPU ID。
pub(super) fn select_cpu(task: &TaskControlBlock, affinity: CpuAffinity) -> CpuId {
    select_cpu_with(task, affinity, false)
}

/// 被抢占的 task 重新进入 Ready 时选核；每隔固定次数的抢占放宽 last-CPU 亲和偏置。
///
/// 放宽只在本 CPU 上还有别的可运行任务时才生效：此时负载并列意味着两个 CPU 的竞争程度相同，
/// 轮转才能让 N+1 个 CPU 密集任务在 N 个 CPU 上长期公平；缺少排队任务时放宽只会让单个 yield 的
/// task 无谓地来回迁移。策略见 `scheduler::placement_policy`。
///
/// # Parameters
///
/// - `task`: 刚被抢占、尚未重新入队的 task。
/// - `affinity`: 调用方从同一 SchedulingState transaction 取得的 CPU 集合。
///
/// # Returns
///
/// 被选中的 CPU ID。
pub(super) fn select_cpu_after_preemption(task: &TaskControlBlock, affinity: CpuAffinity) -> CpuId {
    let slot = current_per_cpu();
    // OWNER 见 `PerCpuProcessor::preemptions_since_rotation`：只有本 CPU 读写。
    let (due, next) =
        placement_policy::advance_rotation(slot.preemptions_since_rotation.load(Ordering::Relaxed));
    slot.preemptions_since_rotation
        .store(next, Ordering::Relaxed);
    // 此刻被抢占的 task 已离开 Running，而它的继任者已从 Ready 变成 Running，所以“还有别的任务在
    // 本 CPU 上竞争”要看 Ready 与 Running 之和，只看 Ready 在两个任务轮流跑时恒为零。
    let contended = slot
        .ready_entries
        .load(Ordering::Relaxed)
        .saturating_add(slot.running_entries.load(Ordering::Relaxed))
        != 0;
    select_cpu_with(task, affinity, due && contended)
}

fn select_cpu_with(task: &TaskControlBlock, affinity: CpuAffinity, rotate: bool) -> CpuId {
    // Relaxed 只用于分散扫描起点，不承担任何状态发布。
    let start = NEXT_CPU.fetch_add(1, Ordering::Relaxed) % cpu::count();
    let current = cpu::current_id();
    // last_cpu 仅提供缓存亲和性提示；过期值只影响候选顺序，不影响任务所有权或可见性。
    let last = task.scheduling.last_cpu.load(Ordering::Relaxed);
    let candidates = (0..cpu::count()).filter_map(|offset| {
        let index = (start + offset) % cpu::count();
        let cpu_id = cpu::id_at(index).expect("logical CPU disappeared from topology");
        if !cpu::is_active(cpu_id) || !affinity.allows(cpu_id) {
            return None;
        }
        let slot = processor_at(index);
        let load = slot
            .ready_entries
            .load(Ordering::Relaxed)
            .saturating_add(slot.running_entries.load(Ordering::Relaxed));
        Some((index, load))
    });
    // 并列时保留 last 的理由：允许多一个 runnable 会把两个 CPU-bound task 永久压在同一 CPU，
    // 所以只在负载相同（而非 last 多一个）时才留下；rotate 则周期性地打破这个并列。
    match placement_policy::choose_cpu(candidates, Some(last), rotate) {
        Some(index) => cpu::id_at(index).expect("logical CPU disappeared from topology"),
        None => {
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
        }
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

/// 将新建 Task 从 New 转换为唯一 Ready membership，并按目标 CPU 的
/// Ready/inbound vruntime floor 完成公平 placement。
///
/// # Parameters
///
/// - `task`: process graph 已拥有的初始 Task。
///
/// # Returns
///
/// 选中的 CPU。
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
