use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Ready,
    Preempting,
    Blocking,
    WakePending,
    Blocked,
    Stopped,
}

#[derive(Debug, Default)]
struct Scheduler {
    states: BTreeMap<usize, State>,
    vruntime: BTreeMap<usize, u64>,
    current: Option<usize>,
    ready: VecDeque<usize>,
    pending: Option<usize>,
    reschedule: bool,
    switches: usize,
    idle_entries: usize,
    completions: usize,
}

impl Scheduler {
    fn with_running_and_ready(current: usize, ready: usize) -> Self {
        Self {
            states: BTreeMap::from([(current, State::Running), (ready, State::Ready)]),
            vruntime: BTreeMap::from([(current, 0), (ready, 0)]),
            current: Some(current),
            ready: VecDeque::from([ready]),
            ..Self::default()
        }
    }

    fn preempt(&mut self) -> usize {
        let outgoing = self.current.take().unwrap();
        assert_eq!(
            self.states.insert(outgoing, State::Preempting),
            Some(State::Running)
        );
        outgoing
    }

    fn block(&mut self) -> usize {
        let outgoing = self.current.take().unwrap();
        assert_eq!(
            self.states.insert(outgoing, State::Blocking),
            Some(State::Running)
        );
        outgoing
    }

    fn wake(&mut self, task: usize) -> bool {
        match self.states.get(&task) {
            Some(State::Blocking) => {
                self.states.insert(task, State::WakePending);
                true
            }
            Some(State::Blocked) => {
                self.states.insert(task, State::Ready);
                self.ready.push_back(task);
                true
            }
            _ => false,
        }
    }

    fn stop(&mut self, task: usize) {
        assert!(matches!(self.states.get(&task), Some(State::Preempting)));
        self.states.insert(task, State::Stopped);
    }

    fn handoff(&mut self, outgoing: usize) {
        assert!(self.pending.is_none());
        if let Some(next) = self.ready.pop_front() {
            assert_eq!(self.states.insert(next, State::Running), Some(State::Ready));
            self.current = Some(next);
            self.pending = Some(outgoing);
            self.switches += 1;
            return;
        }
        if matches!(
            self.states.get(&outgoing),
            Some(State::Preempting | State::WakePending)
        ) {
            self.states.insert(outgoing, State::Running);
            self.current = Some(outgoing);
            return;
        }
        self.pending = Some(outgoing);
        self.switches += 1;
        self.idle_entries += 1;
    }

    fn complete(&mut self) {
        let Some(outgoing) = self.pending.take() else {
            return;
        };
        match self.states[&outgoing] {
            State::Preempting | State::WakePending => {
                self.states.insert(outgoing, State::Ready);
                self.ready.push_back(outgoing);
                let current = self.current.expect("handoff completion requires current");
                self.reschedule = super::preemption_policy::local_ready_preempts(
                    Some(self.vruntime[&current]),
                    Some(self.vruntime[&outgoing]),
                );
            }
            State::Blocking => {
                self.states.insert(outgoing, State::Blocked);
            }
            State::Stopped => {}
            state => panic!("invalid pending handoff state {state:?}"),
        }
        self.completions += 1;
    }

    fn assert_single_running_owner(&self) {
        assert_eq!(
            self.states
                .values()
                .filter(|state| **state == State::Running)
                .count(),
            usize::from(self.current.is_some())
        );
    }
}

#[test]
fn runnable_ping_pong_uses_one_switch_per_handoff() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    for _ in 0..1_024 {
        let outgoing = scheduler.preempt();
        scheduler.handoff(outgoing);
        scheduler.complete();
        scheduler.assert_single_running_owner();
    }
    assert_eq!(scheduler.switches, 1_024);
    assert_eq!(scheduler.idle_entries, 0);
    assert_eq!(scheduler.completions, 1_024);
}

#[test]
fn wake_race_is_claimed_once_after_context_save() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    let outgoing = scheduler.block();
    assert!(scheduler.wake(outgoing));
    assert!(!scheduler.wake(outgoing), "second completion must be stale");
    scheduler.handoff(outgoing);
    assert_eq!(scheduler.states[&outgoing], State::WakePending);
    scheduler.complete();
    scheduler.complete();
    assert_eq!(scheduler.states[&outgoing], State::Ready);
    assert_eq!(scheduler.completions, 1);
}

#[test]
fn idle_is_used_only_for_a_non_runnable_outgoing_task() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    scheduler.ready.clear();
    scheduler.states.remove(&2);
    let outgoing = scheduler.preempt();
    scheduler.handoff(outgoing);
    assert_eq!((scheduler.switches, scheduler.idle_entries), (0, 0));

    let outgoing = scheduler.block();
    scheduler.handoff(outgoing);
    assert_eq!((scheduler.switches, scheduler.idle_entries), (1, 1));
    scheduler.complete();
    assert_eq!(scheduler.states[&outgoing], State::Blocked);
}

#[test]
fn stop_consequence_is_not_resumed_as_a_self_yield() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    scheduler.ready.clear();
    scheduler.states.remove(&2);
    let outgoing = scheduler.preempt();
    scheduler.stop(outgoing);
    scheduler.handoff(outgoing);
    assert_eq!(scheduler.idle_entries, 1);
    scheduler.complete();
    assert_eq!(scheduler.states[&outgoing], State::Stopped);
}

#[test]
fn higher_vruntime_outgoing_cannot_immediately_repreempt_successor() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    scheduler.vruntime.insert(1, 1_000_000);
    scheduler.vruntime.insert(2, 10_000);

    let outgoing = scheduler.preempt();
    scheduler.handoff(outgoing);
    scheduler.complete();

    assert_eq!(scheduler.current, Some(2));
    assert_eq!(scheduler.states[&outgoing], State::Ready);
    assert!(!scheduler.reschedule);
}

#[test]
fn earlier_ready_entity_requests_policy_preemption() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    scheduler.vruntime.insert(1, 1_000);
    scheduler.vruntime.insert(2, 20_000);

    let outgoing = scheduler.preempt();
    scheduler.handoff(outgoing);
    scheduler.complete();

    assert_eq!(scheduler.current, Some(2));
    assert!(scheduler.reschedule);
}

#[test]
fn equal_vruntime_does_not_create_preemption_ping_pong() {
    let mut scheduler = Scheduler::with_running_and_ready(1, 2);
    scheduler.vruntime.insert(1, 50_000);
    scheduler.vruntime.insert(2, 50_000);

    let outgoing = scheduler.preempt();
    scheduler.handoff(outgoing);
    scheduler.complete();

    assert!(!scheduler.reschedule);
}
