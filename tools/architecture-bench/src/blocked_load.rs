#![feature(allocator_ext)]
//! Linux scheduling-state admission, retirement and load sampling on the actual owner.
extern crate alloc;
mod cost_policy;
include!("../../kernel-unit/src/linux_scheduling_fixture.rs");
use std::{hint::black_box, time::Instant};
fn measure(name: &str, operation: &mut dyn FnMut()) {
    for _ in 0..cost_policy::ITERATIONS {
        operation();
    }
    let mut samples = [0.0f64; cost_policy::SAMPLES];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..cost_policy::ITERATIONS {
            black_box(&mut *operation)();
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
    task::benchmark(measure);
}
