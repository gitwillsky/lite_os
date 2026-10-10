//! 实际 shared page ledger 的一致快照和 physical transition CPU 成本。
mod cost_policy;
use std::{hint::black_box, time::Instant};
#[path = "../../../kernel/src/memory/shared_memory"]
mod memory {
    mod statistics;
    pub(crate) fn benchmark() {
        use statistics::{SharedPageResidency as Residency, SharedPageStatistics};
        for pages in [1, 1024, 1_048_576] {
            let ledger = SharedPageStatistics::new();
            ledger.admit(Residency::Discarded);
            ledger.retire(Residency::Discarded);
            assert_eq!(ledger.snapshot(), (0, 0));
            for _ in 0..pages {
                ledger.admit(Residency::Resident);
            }
            crate::measure(
                &format!("shared page aggregate read, {pages} resident memberships"),
                || {
                    std::hint::black_box(ledger.snapshot());
                },
            );
            crate::measure(
                &format!("shared page physical swap/fault transaction, {pages} memberships"),
                || {
                    ledger.transition(Residency::Resident, Residency::Swapped);
                    ledger.transition(Residency::Swapped, Residency::Resident);
                },
            );
            crate::measure("shared page indexed admission and final retirement", || {
                ledger.admit(Residency::Resident);
                ledger.retire(Residency::Resident);
            });
            assert_eq!(ledger.snapshot(), (pages, 0));
            for _ in 0..pages {
                ledger.retire(Residency::Resident);
            }
            assert_eq!(ledger.snapshot(), (0, 0));
        }
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
    memory::benchmark();
}
