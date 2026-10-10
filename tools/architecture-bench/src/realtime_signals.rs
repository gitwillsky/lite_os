#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 RT record publication/dequeue 热路径；allocator/ucounts preparation 与最终退款移出计时区。
extern crate alloc;
mod cost_policy;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/file/signalfd.rs"]
mod file;
#[path = "../../kernel-unit/src/namespace_fixture.rs"]
mod namespace;
mod sync {
    pub(crate) fn next_readiness_generation() -> u64 {
        static GENERATION: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        GENERATION.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }
}
#[path = "../../../kernel/src/task/model"]
mod task {
    mod signal_info;
    use signal_info::PendingSignal;
    mod signal_pending;
    pub(crate) fn benchmark() {
        use signal_pending::{PendingSignals, PreparedSignalRecord};
        const BATCH: usize = 1024;
        crate::namespace::boot();
        // 实际 native codec 与共享 mask 投影；无动态 callback、无 staging 分配。
        let descriptor = crate::file::SignalFd::try_new(u64::MAX).unwrap();
        for (signal, info) in [
            (35, PendingSignal::timer(0, 7, 0x11223344ffff0081)),
            (17, PendingSignal::child_exited(42, 23)),
            (31, PendingSignal::seccomp(6, 63, 0xc00000b7, 0x12345678)),
        ] {
            let mut samples = [0.0f64; crate::cost_policy::SAMPLES];
            for sample in &mut samples {
                let start = std::time::Instant::now();
                for _ in 0..crate::cost_policy::ITERATIONS {
                    std::hint::black_box(descriptor.mask());
                    std::hint::black_box(std::hint::black_box(info).encode_signalfd(signal));
                }
                *sample = start.elapsed().as_nanos() as f64 / crate::cost_policy::ITERATIONS as f64;
            }
            samples.sort_by(f64::total_cmp);
            let median = samples[samples.len() / 2];
            println!("signalfd mask/native codec, signal {signal}: {median:.2} ns/op");
            assert!(
                median <= crate::cost_policy::MAX_NANOSECONDS_PER_OPERATION,
                "signalfd native codec existing hot-path policy exceeded"
            );
        }
        // 实际 POSIX 预留节点 roundtrip：checkout/发布/消费/归还，无 dynamic ucounts/allocator。
        for signal in [14, 35] {
            let owner = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                591402,
                1,
                (92, 0),
            )
            .unwrap();
            let pending = spin::Mutex::new(PendingSignals::new());
            let mut samples = [0.0f64; crate::cost_policy::SAMPLES];
            for sample in &mut samples {
                let start = std::time::Instant::now();
                for sequence in 0..crate::cost_policy::ITERATIONS {
                    let mut record = owner.prepare(sequence);
                    pending
                        .lock()
                        .queue(signal, PendingSignal::timer(0, 0, 42), &mut record);
                    let selected = pending
                        .lock()
                        .take(u64::MAX)
                        .expect("actual preallocated record");
                    std::hint::black_box(
                        selected
                            .resolve(|_, _| true)
                            .expect("valid timer notification"),
                    );
                }
                *sample = start.elapsed().as_nanos() as f64 / crate::cost_policy::ITERATIONS as f64;
            }
            samples.sort_by(f64::total_cmp);
            let median = samples[samples.len() / 2];
            println!("POSIX notification reuse, signal {signal}: {median:.2} ns/roundtrip");
            assert!(
                median <= crate::cost_policy::MAX_NANOSECONDS_PER_OPERATION,
                "POSIX preallocated notification existing hot-path policy exceeded"
            );
            assert_eq!(pending.lock().bits, 0);
            drop(owner);
        }
        // ordinary 已有实际节点时的 publication 只 coalesce；unused prepared storage 在计时外退款。
        {
            const BATCH: usize = 1024;
            let info = PendingSignal::thread_directed(42);
            let prepare = || {
                PreparedSignalRecord::prepare(
                    10,
                    info,
                    crate::namespace::initial_user(),
                    591403,
                    u64::MAX,
                )
            };
            let pending = spin::Mutex::new(PendingSignals::new());
            pending.lock().queue(10, info, &mut prepare());
            let mut prepared = Vec::with_capacity(BATCH);
            let mut samples = [0.0f64; crate::cost_policy::SAMPLES];
            for sample in &mut samples {
                let mut elapsed = std::time::Duration::ZERO;
                let mut completed = 0;
                while completed < crate::cost_policy::ITERATIONS {
                    let count = BATCH.min((crate::cost_policy::ITERATIONS - completed) as usize);
                    for _ in 0..count {
                        let record = prepare();
                        assert!(record.has_record());
                        prepared.push(record);
                    }
                    let start = std::time::Instant::now();
                    for record in &mut prepared {
                        pending.lock().queue(10, info, record);
                    }
                    elapsed += start.elapsed();
                    prepared.clear();
                    completed += count as u64;
                }
                *sample = elapsed.as_nanos() as f64 / crate::cost_policy::ITERATIONS as f64;
            }
            samples.sort_by(f64::total_cmp);
            let median = samples[samples.len() / 2];
            println!("ordinary pending coalescing: {median:.2} ns/op");
            assert!(
                median <= crate::cost_policy::MAX_NANOSECONDS_PER_OPERATION,
                "ordinary pending existing hot-path policy exceeded"
            );
            let selected = pending.lock().take(u64::MAX).unwrap();
            assert_eq!(selected.into_parts().1.encode(10), info.encode(10));
            assert_eq!(pending.lock().bits, 0);
        }
        for depth in [1, 1024] {
            let pending = spin::Mutex::new(PendingSignals::new());
            let prepare = || {
                let record = PreparedSignalRecord::prepare(
                    35,
                    PendingSignal::timer(1, 0, 42),
                    crate::namespace::initial_user(),
                    591401,
                    u64::MAX,
                );
                assert!(!record.unavailable_for(35, PendingSignal::timer(1, 0, 42)));
                record
            };
            for _ in 0..depth {
                pending
                    .lock()
                    .queue(35, PendingSignal::timer(1, 0, 42), &mut prepare());
            }
            let mut samples = [[0.0f64; crate::cost_policy::SAMPLES]; 2];
            let mut selected = Vec::with_capacity(BATCH);
            let mut prepared = Vec::with_capacity(BATCH);
            for sample in 0..crate::cost_policy::SAMPLES {
                let mut publication = std::time::Duration::ZERO;
                let mut consumption = std::time::Duration::ZERO;
                let mut completed = 0;
                while completed < crate::cost_policy::ITERATIONS {
                    let count = BATCH.min((crate::cost_policy::ITERATIONS - completed) as usize);
                    for _ in 0..count {
                        prepared.push(prepare());
                    }
                    let start = std::time::Instant::now();
                    for record in &mut prepared {
                        pending.lock().queue(
                            35,
                            std::hint::black_box(PendingSignal::timer(1, 0, 42)),
                            record,
                        );
                    }
                    publication += start.elapsed();
                    let start = std::time::Instant::now();
                    for _ in 0..count {
                        selected.push(std::hint::black_box(
                            pending
                                .lock()
                                .take(u64::MAX)
                                .expect("actual FIFO membership"),
                        ));
                    }
                    consumption += start.elapsed();
                    // 执行 actual record/ucounts Drop，不将最后退款或分配伪装成 pending spin 热路径。
                    selected.clear();
                    prepared.clear();
                    completed += count as u64;
                }
                samples[0][sample] =
                    publication.as_nanos() as f64 / crate::cost_policy::ITERATIONS as f64;
                samples[1][sample] =
                    consumption.as_nanos() as f64 / crate::cost_policy::ITERATIONS as f64;
            }
            for (name, mut samples) in ["publication", "consumption"].into_iter().zip(samples) {
                samples.sort_by(f64::total_cmp);
                let median = samples[samples.len() / 2];
                println!("RT pending {name}, {depth} resident records: {median:.2} ns/op");
                assert!(
                    median <= crate::cost_policy::MAX_NANOSECONDS_PER_OPERATION,
                    "RT pending existing hot-path policy exceeded"
                );
            }
            let retired = pending.lock().discard(u64::MAX);
            drop(retired);
            assert_eq!(pending.lock().bits, 0);
        }
    }
}
fn main() {
    task::benchmark();
}
