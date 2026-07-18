use std::hint::black_box;
use std::time::{Duration, Instant};

#[path = "../../../kernel/src/arch/riscv64/pte.rs"]
mod riscv_pte;
#[path = "../../../kernel/src/arch/riscv64/sv39.rs"]
mod sv39;
#[path = "../../../kernel/src/timer/deadline.rs"]
mod timer_deadline;

const ITERATIONS: u64 = 2_000_000;
const SAMPLES: usize = 5;
const MAX_NANOSECONDS_PER_OPERATION: f64 = 200.0;

fn sample(mut operation: impl FnMut(u64) -> usize) -> Duration {
    let started = Instant::now();
    let mut checksum = 0usize;
    for iteration in 0..ITERATIONS {
        checksum ^= operation(black_box(iteration));
    }
    black_box(checksum);
    started.elapsed()
}

fn median(mut samples: [Duration; SAMPLES]) -> Duration {
    samples.sort_unstable();
    samples[SAMPLES / 2]
}

fn verify(name: &str, operation: impl FnMut(u64) -> usize + Copy) {
    // 1. 先预热同一 release code path，避免首次调度或页错误污染样本。
    black_box(sample(operation));
    // 2. 取五次样本的中位数，单次宿主调度抖动不会直接击穿门禁。
    let elapsed = median(std::array::from_fn(|_| sample(operation)));
    let nanoseconds_per_operation = elapsed.as_nanos() as f64 / ITERATIONS as f64;
    println!("{name}: {nanoseconds_per_operation:.2} ns/op");
    // 3. 该上限远高于当前纯整数实现，但能阻止锁、分配或运行时分派进入热路径。
    assert!(
        nanoseconds_per_operation <= MAX_NANOSECONDS_PER_OPERATION,
        "{name} regressed to {nanoseconds_per_operation:.2} ns/op (limit {MAX_NANOSECONDS_PER_OPERATION:.2} ns/op)"
    );
}

fn main() {
    verify("timer deadline", |iteration| {
        let previous = 10_000 + iteration % 997;
        let now = previous + iteration % 31;
        timer_deadline::next(previous, now, 7).expect("benchmark input must be valid") as usize
    });
    verify("Sv39 indexes", |iteration| {
        let indexes = sv39::indexes((iteration as usize).wrapping_mul(0x9e37_79b9));
        indexes[0] ^ indexes[1] ^ indexes[2]
    });
    verify("semantic PTE encoding", |iteration| {
        let mut permissions = riscv_pte::PagePermissions::READ;
        if iteration & 1 != 0 {
            permissions |= riscv_pte::PagePermissions::WRITE;
        }
        let encoded = riscv_pte::encode(permissions).expect("benchmark permissions must be valid");
        riscv_pte::decode(encoded).bits() as usize
    });
}
