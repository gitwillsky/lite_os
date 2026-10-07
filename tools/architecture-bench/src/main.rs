use std::hint::black_box;
use std::time::{Duration, Instant};

#[path = "../../../kernel/src/hardware/arch/aarch64/pte.rs"]
mod aarch64_pte;
#[path = "../../../kernel/src/timer/deadline.rs"]
mod timer_deadline;
#[path = "../../../kernel/src/hardware/arch/aarch64/va39.rs"]
mod va39;

#[path = "mmio_fixture.rs"]
mod arch;
#[allow(dead_code)]
#[path = "../../../kernel/src/hardware/hal/bus.rs"]
mod mmio_bus;

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
    // 只测 production window validation 的 CPU 成本；buffer 不代表设备延迟或 IRQ 时序。
    let mut registers = [0u64; 16];
    let bus = mmio_bus::MmioBus::new(registers.as_mut_ptr() as usize, size_of_val(&registers))
        .expect("benchmark buffer must be valid");
    verify("HAL MMIO checked read/write", |iteration| {
        let offset = black_box((iteration as usize % 16) * 8);
        let bus = black_box(&bus);
        bus.write_u64(offset, iteration).expect("aligned register");
        bus.read_u64(offset).expect("aligned register") as usize
    });
    verify("timer deadline", |iteration| {
        let previous = 10_000 + iteration % 997;
        let now = previous + iteration % 31;
        timer_deadline::next(previous, now, 7).expect("benchmark input must be valid") as usize
    });
    verify("AArch64 VA39 indexes", |iteration| {
        let indexes = va39::indexes((iteration as usize).wrapping_mul(0x9e37_79b9));
        indexes[0] ^ indexes[1] ^ indexes[2]
    });
    verify("AArch64 TLBI operand", |iteration| {
        va39::tlbi_all_asid_operand(
            0xffff_ffc0_0000_0000usize | (iteration as usize).wrapping_mul(4096),
        ) as usize
    });
    verify("AArch64 semantic PTE encoding", |iteration| {
        let mut permissions = aarch64_pte::PagePermissions::READ;
        if iteration & 1 != 0 {
            permissions |= aarch64_pte::PagePermissions::WRITE;
        }
        let encoded =
            aarch64_pte::encode(permissions).expect("benchmark permissions must be valid");
        aarch64_pte::decode(encoded | aarch64_pte::TABLE_OR_PAGE).bits() as usize
    });
}
