#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 实际 INET owner 的 TCP lifecycle、UDP ingress/readiness/loan 与有界扫描成本。
extern crate alloc;
include!("../../kernel-unit/src/network_endpoint_fixture.rs");
mod cost_policy;
fn main() {
    lifecycle_benchmark(|name, iterations, handles, gated, operation| {
        for _ in 0..iterations {
            operation();
        }
        let mut samples = [0.0f64; cost_policy::SAMPLES];
        for sample in &mut samples {
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                operation();
            }
            *sample = start.elapsed().as_nanos() as f64 / iterations as f64 / handles as f64;
        }
        samples.sort_by(f64::total_cmp);
        let median = samples[samples.len() / 2];
        println!("{name}: {median:.2} ns/op");
        if gated {
            assert!(
                median <= cost_policy::MAX_NANOSECONDS_PER_OPERATION,
                "{name}: hot-path policy exceeded"
            );
        }
    });
}
