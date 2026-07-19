use std::{fs, path::Path};

const MODEL_SOURCE: &str = "kernel/src/task/model.rs";
const LIMIT_SOURCE: &str = "kernel/src/task/model/resource_limits.rs";
const READY_QUEUE_SOURCE: &str = "kernel/src/task/processor/ready_queue.rs";
const PREEMPTION_POLICY_SOURCE: &str = "kernel/src/task/scheduler/preemption_policy.rs";
const SWITCHES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SchedulerCost {
    unlimited_cpu_limit_locks: usize,
    unconditional_local_ready_preemptions: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(SchedulerCost {
            unlimited_cpu_limit_locks: 0,
            unconditional_local_ready_preemptions: 0,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{LIMIT_SOURCE}: default-unlimited RLIMIT_CPU must not lock on context switch; S={SWITCHES}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<SchedulerCost, String> {
    let model = read(root, MODEL_SOURCE)?;
    let limits = read(root, LIMIT_SOURCE)?;
    let ready_queue = read(root, READY_QUEUE_SOURCE)?;
    let preemption_policy = read(root, PREEMPTION_POLICY_SOURCE)?;
    let legacy = limits.contains("self.process.resource_limits.lock().cpu_signal(runtime_us)")
        && !model.contains("cpu_limit_active: AtomicBool");
    if legacy {
        return Ok(SchedulerCost {
            unlimited_cpu_limit_locks: SWITCHES,
            unconditional_local_ready_preemptions: measure_ready_preemptions(
                &ready_queue,
                &preemption_policy,
            )?,
        });
    }
    let fast_path = model.contains("cpu_limit_active: AtomicBool")
        && limits.contains("if !self.process.cpu_limit_active.load(")
        && limits.contains("return None;")
        && limits.contains("self.process.resource_limits.lock().cpu_signal(runtime_us)");
    if fast_path {
        return Ok(SchedulerCost {
            unlimited_cpu_limit_locks: 0,
            unconditional_local_ready_preemptions: measure_ready_preemptions(
                &ready_queue,
                &preemption_policy,
            )?,
        });
    }
    Err(format!(
        "{MODEL_SOURCE}: RLIMIT_CPU ownership/fast-path seam is not recognized"
    ))
}

fn measure_ready_preemptions(ready_queue: &str, policy: &str) -> Result<usize, String> {
    if ready_queue.contains("slot.running_entries.load(Ordering::Relaxed) != 0")
        || ready_queue.contains("publish_reschedule_at(cpu_id);")
    {
        return Ok(SWITCHES);
    }
    if ready_queue.contains("processor.runqueue.minimum_vruntime()")
        && ready_queue.contains("platform::send_ipi(CpuSet::singleton(cpu_id))")
        && ready_queue
            .contains("if with_current_processor(|processor| processor.add_ready_entry(entry))")
        && policy.contains(".is_some_and(|(current, ready)| ready < current)")
    {
        return Ok(0);
    }
    Err(format!(
        "{READY_QUEUE_SOURCE}: local Ready preemption policy is not recognized"
    ))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_cpu_limit_has_no_context_switch_lock() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production scheduler cost must be measurable");
        assert_eq!(
            cost.unlimited_cpu_limit_locks, 0,
            "S={SWITCHES}, measured {cost:?}"
        );
        assert_eq!(
            cost.unconditional_local_ready_preemptions, 0,
            "S={SWITCHES}, measured {cost:?}"
        );
    }
}
