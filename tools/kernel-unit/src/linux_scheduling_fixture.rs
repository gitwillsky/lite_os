// Host syscall/TCB boundaries; Linux run-state accounting executes the production owner.
#[path = "../../../system/linuxd/src/cpu.rs"]
#[expect(dead_code, reason = "host accounting does not query current CPU or enumerate affinity")]
pub(crate) mod cpu;
#[path = "sync.rs"]
#[expect(dead_code, reason = "accounting fixture needs wait identities, not driver readiness or parking")]
pub(crate) mod sync;
struct SystemInfo { cpu_count: u32 }
fn system_info() -> SystemInfo { SystemInfo { cpu_count: 2 } }

#[expect(dead_code, reason = "TCB runtime projections are assembled only to type-check the actual scheduling module")]
#[path = "../../../system/linuxd/src/task/model"]
pub(crate) mod task {
    use alloc::sync::Arc;
    use crate::sync::Mutex;
    pub(crate) struct Process {
        start_time_us: u64,
        leader_scheduler: Mutex<Arc<scheduling::SchedulingEntity>>,
    }
    pub(crate) struct Execution { start_time_us: u64 }
    pub(crate) struct TaskControlBlock {
        scheduling: Arc<scheduling::SchedulingEntity>,
        execution: Execution,
    }
    impl TaskControlBlock {
        fn is_kernel_thread(&self) -> bool { unreachable!("runtime projection seam") }
        fn tid(&self) -> usize { unreachable!("runtime projection seam") }
        fn tgid(&self) -> usize { unreachable!("runtime projection seam") }
        fn process(&self) -> &Process { unreachable!("runtime projection seam") }
        fn thread_cpu_time(&self) -> (u64, u64) { unreachable!("runtime projection seam") }
    }
    #[path = "scheduling.rs"]
    mod scheduling;

    pub(crate) fn benchmark(measure: impl Fn(&str, &mut dyn FnMut())) {
        use scheduling::{CpuAffinity, RunState, SchedulingState, WaitMembership};
        let blocked = || {
            let mut state = SchedulingState::new(CpuAffinity::all_possible());
            state.wait = Some(WaitMembership::Vfork(1));
            state.set_run_state(RunState::Blocked);
            state
        };
        let held: alloc::vec::Vec<_> = (0..1024).map(|_| blocked()).collect();
        measure("linuxd blocked membership register/retire with 1024 admissions", &mut || {
            drop(std::hint::black_box(blocked()));
        });
        measure("linuxd actual blocked aggregate sample with 1024 admissions", &mut || {
            std::hint::black_box(scheduling::uninterruptible_task_count());
        });
        drop(held);
        assert_eq!(scheduling::uninterruptible_task_count(), 0);
    }

    #[test]
    fn run_stop_wait_and_drop_retire_each_aggregate_once() {
        use scheduling::*;
        assert_eq!(uninterruptible_task_count(), 0);
        assert_eq!(runnable_task_count(), 0);
        let mut state = SchedulingState::new(CpuAffinity::all_possible());
        state.set_run_state(RunState::Running);
        state.set_run_state(RunState::StopPending);
        assert_eq!(runnable_task_count(), 1);
        state.wait = Some(WaitMembership::Vfork(7));
        state.set_run_state(RunState::Blocked);
        state.set_run_state(RunState::Blocked);
        assert!(state.is_uninterruptible());
        assert_eq!(uninterruptible_task_count(), 1);
        assert_eq!(runnable_task_count(), 0);
        state.set_run_state(RunState::Stopped { resume: StopResume::Blocked });
        assert_eq!(uninterruptible_task_count(), 1);
        state.wait = Some(WaitMembership::Deadline(99));
        state.set_run_state(RunState::Blocked);
        assert!(!state.is_uninterruptible());
        assert_eq!(uninterruptible_task_count(), 0);
        state.wait = Some(WaitMembership::Vfork(8));
        state.set_run_state(RunState::Blocked);
        drop(state);
        assert_eq!(uninterruptible_task_count(), 0);
        let mut running = SchedulingState::new(CpuAffinity::all_possible());
        running.set_run_state(RunState::Running);
        drop(running);
        assert_eq!(runnable_task_count(), 0);
    }
}
