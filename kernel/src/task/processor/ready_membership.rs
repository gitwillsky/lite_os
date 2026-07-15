use super::*;

fn processor_for_hart(cpu: usize) -> &'static PerHartProcessor {
    let index = if cpu == hart_id() {
        hart::current_hart_index()
    } else {
        hart::hart_index(cpu)
            .unwrap_or_else(|| panic!("scheduler CPU {} is absent from DTB topology", cpu))
    };
    processor_at(index)
}

#[inline(always)]
fn publish(cpu: usize) {
    let slot = processor_for_hart(cpu);
    let previous = slot.ready_entries.fetch_add(1, Ordering::Relaxed);
    assert!(
        previous < slot.queue_capacity,
        "Ready membership exceeds fixed-stack capacity"
    );
}

/// @description 消费线性 Ready token 并提交唯一 per-hart logical-load projection。
/// @param transition SchedulingState lock 内刚产生且尚未消费的 token。
/// @return 本次 Ready generation，供同一 transaction 构造物理 queue token。
#[inline(always)]
pub(super) fn commit_ready_transition(transition: ReadyTransition<'_>) -> u64 {
    let (previous, target, generation) = transition.consume_ready_projection_parts();
    match previous {
        Some(source) if source == target => {}
        Some(source) => {
            // Relaxed readers may observe the two-instruction move; publish target first keeps a
            // concurrent load choice conservative instead of momentarily hiding a runnable task.
            publish(target);
            retire(source);
        }
        None => publish(target),
    }
    generation
}

#[inline(always)]
fn retire(cpu: usize) {
    let previous = processor_for_hart(cpu)
        .ready_entries
        .fetch_sub(1, Ordering::Relaxed);
    assert_ne!(previous, 0, "Ready membership count underflow");
}

/// @description 消费线性 Ready-retirement token 并撤销 per-hart logical-load projection。
/// @param retirement SchedulingState lock 内刚产生且尚未消费的 token。
#[inline(always)]
pub(super) fn commit_ready_retirement(retirement: ReadyRetirement<'_>) {
    retire(retirement.consume_ready_projection_cpu());
}
