//! Host CPU startup and IRQ-lock seams; state transitions and accounting execute kernel code.
pub(crate) mod arch {
    pub(crate) mod cpu {
        pub(crate) struct StartupCpu;
        impl StartupCpu { pub(crate) fn new(_: usize, _: usize) -> Self { Self } }
        pub(crate) fn initialize_startup(_: impl Iterator<Item = StartupCpu>) {}
        pub(crate) fn install_boot_cpu(_: usize) {}
        pub(crate) fn current_logical_id() -> usize { 0 }
        pub(crate) fn entry_identity() -> usize { 0 }
    }
}
#[path = "native_execution.rs"]
pub(crate) mod execution;
pub(crate) mod processor {
    pub(crate) fn account_current_cpu_runtime(_: u64) {}
}
#[expect(dead_code, reason = "the fixture exercises state publication and CPU accounting, not the context switcher")]
#[path = "../../../kernel/src/thread/scheduling.rs"]
mod scheduling;

#[cfg(test)]
mod tests {
    use super::{execution::cpu, scheduling::*};

    fn entity() -> SchedulingEntity {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| cpu::initialize([cpu::HardwareCpuId::from_raw(0), cpu::HardwareCpuId::from_raw(7)], cpu::HardwareCpuId::from_raw(0)));
        SchedulingEntity::new(DEFAULT_PRIORITY, 0, 0)
    }

    #[test]
    fn migration_replaces_generation_and_retires_exact_membership() {
        let entity = entity();
        let mut state = entity.state.lock();
        let cpu0 = cpu::id_at(0).unwrap();
        let cpu1 = cpu::id_at(1).unwrap();
        let (previous, target, old) = state.transition_to_ready(cpu0).consume_ready_projection_parts();
        assert_eq!((previous, target), (None, cpu0));
        let (previous, target, current) = state.transition_to_ready(cpu1).consume_ready_projection_parts();
        assert_eq!((previous, target), (Some(cpu0), cpu1));
        assert_ne!(old, current);
        assert_eq!(state.transition_ready_to_running(cpu1, current).consume_ready_projection_cpu(), cpu1);
        assert_eq!(state.run_state(), RunState::Running { cpu: cpu1 });
    }

    #[test]
    #[should_panic(expected = "selected queue token lost Ready ownership")]
    fn stale_queue_generation_cannot_dispatch_a_migrated_thread() {
        let entity = entity();
        let mut state = entity.state.lock();
        let cpu0 = cpu::id_at(0).unwrap();
        let (_, _, old) = state.transition_to_ready(cpu0).consume_ready_projection_parts();
        state.transition_to_ready(cpu::id_at(1).unwrap()).consume_ready_projection_parts();
        state.transition_ready_to_running(cpu0, old).consume_ready_projection_cpu();
    }

    #[test]
    #[should_panic(expected = "Ready state published without per-CPU membership commit")]
    fn dropping_publication_token_fails_before_projection_can_diverge() {
        let entity = entity();
        drop(entity.state.lock().transition_to_ready(cpu::id_at(0).unwrap()));
    }

    #[test]
    #[should_panic(expected = "Ready state retired without per-CPU membership commit")]
    fn dropping_retirement_token_fails_before_projection_can_diverge() {
        let entity = entity();
        let mut state = entity.state.lock();
        let cpu0 = cpu::id_at(0).unwrap();
        let (_, _, generation) = state.transition_to_ready(cpu0).consume_ready_projection_parts();
        drop(state.transition_ready_to_running(cpu0, generation));
    }

    #[test]
    #[should_panic(expected = "Ready membership requires a linear transition token")]
    fn exit_cannot_bypass_ready_membership_retirement() {
        let entity = entity();
        let mut state = entity.state.lock();
        state.transition_to_ready(cpu::id_at(0).unwrap()).consume_ready_projection_parts();
        state.replace_non_ready_state(RunState::Exited);
    }

    #[test]
    fn restricted_checkpoints_charge_each_interval_once() {
        let entity = entity();
        let mut policy = entity.policy.lock();
        policy.begin(0);
        policy.checkpoint(10);
        policy.set_restricted(15, true);
        policy.checkpoint(20);
        policy.set_restricted(25, false);
        policy.finish(30);
        assert_eq!(policy.total_runtime_us(), 30);
        assert_eq!(policy.restricted_runtime_us(), 10);
        assert_eq!(policy.vruntime, 30 * 1024);
    }
}
