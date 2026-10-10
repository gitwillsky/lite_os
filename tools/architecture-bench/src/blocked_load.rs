//! 实际 scheduler blocked admission/retirement 与 aggregate sample 的 CPU 测量。
extern crate alloc;
mod cost_policy;
use std::{hint::black_box, time::Instant};
#[path = "../../../kernel/src/task/model/scheduling"]
mod task {
    mod blocked_load;
    pub(crate) fn benchmark() {
        use blocked_load::{BlockedLoad, uninterruptible_task_count};
        let held: Vec<_> = (0..1024).map(|_| BlockedLoad::register()).collect();
        crate::measure(
            "scheduler blocked membership register/retire with 1024 admissions",
            || {
                drop(std::hint::black_box(BlockedLoad::register()));
            },
        );
        crate::measure(
            "scheduler actual blocked aggregate sample with 1024 admissions",
            || {
                std::hint::black_box(uninterruptible_task_count());
            },
        );
        drop(held);
        assert_eq!(uninterruptible_task_count(), 0);
    }
}
fn measure(name: &str, mut operation: impl FnMut()) {
    for _ in 0..cost_policy::ITERATIONS {
        operation();
    }
    let mut samples = [0.0f64; cost_policy::SAMPLES];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..cost_policy::ITERATIONS {
            black_box(&mut operation)();
        }
        *sample = start.elapsed().as_nanos() as f64 / cost_policy::ITERATIONS as f64;
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    println!("{name}: {median:.2} ns/op");
    assert!(
        median <= cost_policy::MAX_NANOSECONDS_PER_OPERATION,
        "{name}: existing hot-path policy exceeded"
    );
}
fn main() {
    task::benchmark();
}
