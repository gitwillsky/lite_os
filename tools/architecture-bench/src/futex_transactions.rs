#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! actual TaskMutex/source transaction 与空队列 projection；host IRQ 是 adapter seam。
extern crate alloc;
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;
mod cost_policy;
include!("../../kernel-unit/src/futex_owner_fixture.rs");
use std::{hint::black_box, time::Instant};
fn measure(name: &str, mut operation: impl FnMut()) {
    for _ in 0..cost_policy::ITERATIONS {
        operation();
    }
    let mut samples = [0.0; cost_policy::SAMPLES];
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
        "existing hot-path gate exceeded"
    );
}
fn main() {
    let first = memory::FutexKey::Private {
        address_space: 1,
        address: 0x1000,
    };
    let second = memory::FutexKey::Private {
        address_space: 1,
        address: 0x1004,
    };
    measure("actual futex one-source empty wake transaction", || {
        black_box(crate::execution::task::process_table::wake(
            black_box(first),
            1,
            u32::MAX,
        ));
    });
    measure("actual futex two-source empty wake transaction", || {
        crate::execution::task::process_table::with_operation(
            &[black_box(first), black_box(second)],
            |wake| {
                black_box(wake(first, 1, u32::MAX));
                black_box(wake(second, 1, u32::MAX));
            },
        );
    });
}
