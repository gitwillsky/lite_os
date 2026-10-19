#![feature(allocator_ext)]

extern crate alloc;

use std::hint::black_box;
use std::time::{Duration, Instant};

#[path = "../../../kernel/src/arch/aarch64/pte.rs"]
mod aarch64_pte;
#[allow(dead_code)]
#[path = "../../../kernel/src/task/model/syscall_restart.rs"]
mod syscall_restart;
#[path = "../../../kernel/src/timer/deadline.rs"]
mod timer_deadline;
#[path = "../../../kernel/src/arch/aarch64/va39.rs"]
mod va39;

#[allow(dead_code)]
#[path = "../../kernel-unit/src/namespace_fixture.rs"]
mod namespace;

#[path = "mmio_fixture.rs"]
mod arch;
#[allow(dead_code)]
#[path = "../../../kernel/src/hal/bus.rs"]
mod mmio_bus;

macro_rules! info { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
#[allow(dead_code, unused_imports)]
#[path = "../../kernel-unit/src/seccomp_environment.rs"]
mod seccomp_environment;
use seccomp_environment::{file, id};
#[allow(dead_code, unused_imports)]
mod ipc {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kernel-unit/src/ipc_namespace_fixture.rs"
    ));
    pub(crate) use crate::seccomp_environment::ipc::*;
}
#[allow(dead_code, unused_imports)]
#[path = "../../kernel-unit/src/shm_backing_fixture.rs"]
mod memory;
#[allow(dead_code, unused_imports)]
#[path = "../../kernel-unit/src/sync.rs"]
mod sync;
mod task {
    pub(crate) fn current_task() -> Option<alloc::sync::Arc<super::TaskControlBlock>> {
        None
    }
}
#[allow(dead_code)]
#[path = "seccomp_fixture.rs"]
mod model;
use model::seccomp;

#[allow(dead_code)]
struct TaskControlBlock {
    security: seccomp::ThreadSecurityOwner,
}
#[allow(dead_code)]
impl TaskControlBlock {
    fn tid(&self) -> usize {
        1
    }
    fn thread(&self) -> &Self {
        self
    }
    fn user_program_counter(&self) -> usize {
        0x1234
    }
}

#[allow(dead_code)]
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;

#[allow(dead_code)]
#[path = "../../../kernel/src/fs"]
mod fs {
    pub(crate) mod chardev {
        pub(crate) use crate::seccomp_environment::chardev::*;
        pub(crate) trait UserOutput {
            fn remaining(&self) -> usize;
            fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault>;
        }
    }
    #[path = "procfs/ipc_description.rs"]
    mod ipc_description;
    pub(crate) const O_RDWR: u32 = 2;
    pub(crate) const O_CLOEXEC: u32 = 0o2000000;
    #[derive(Debug)]
    pub(crate) enum FileSystemError {
        OutOfMemory,
        InvalidOperation,
        AlreadyExists,
        Busy,
    }
    mod block_claim;

    pub(super) fn benchmark_sysvipc() {
        use crate::ipc::{IpcAccess, IpcNamespace, IpcTable};
        struct Sink(usize);
        impl chardev::UserOutput for Sink {
            fn remaining(&self) -> usize {
                self.0
            }
            fn write(&mut self, bytes: &[u8]) -> Result<(), chardev::UserFault> {
                std::hint::black_box(bytes);
                self.0 -= bytes.len();
                Ok(())
            }
        }
        let authority = crate::namespace::root_authority();
        let access = IpcAccess {
            authority: authority.clone(),
            filesystem_gid: 0,
            groups: None,
        };
        for objects in [16, 1024] {
            let owner = IpcNamespace::create(authority.namespace.clone(), 0).unwrap();
            for key in 1..=objects {
                owner
                    .message_get(key, syscall_abi::ipc::IPC_CREAT | 0o666, &access, 1)
                    .unwrap();
            }
            let mut reader = ipc_description::IpcReadCursor::new();
            let pid = crate::namespace::initial_pid();
            let mut sample = || {
                let started = std::time::Instant::now();
                for _ in 0..500 {
                    let mut output = Sink(usize::MAX);
                    reader
                        .read(
                            &mut output,
                            0,
                            &owner,
                            IpcTable::Message,
                            &authority.namespace,
                            &pid,
                        )
                        .unwrap();
                    std::hint::black_box(output.0);
                }
                started.elapsed().as_nanos() as f64 / 500.0
            };
            std::hint::black_box(sample());
            let mut samples =
                std::array::from_fn::<_, { crate::cost_policy::SAMPLES }, _>(|_| sample());
            samples.sort_by(f64::total_cmp);
            println!(
                "sysvipc actual slot/stack codec full read ({objects} objects): {:.2} ns/op (diagnostic)",
                samples[samples.len() / 2]
            );
        }
    }

    pub(super) fn benchmark_block_claim() {
        use block_claim::BlockClaim;
        use std::hint::black_box;
        let whole = BlockClaim::whole(1, 1 << 20).expect("whole disk extent");
        crate::verify("block claim raw-read cache policy one extent", |_| {
            usize::from(black_box(&whole).bypass_cache())
        });
        let aliases: Vec<_> = (0..8)
            .map(|index| whole.partition(2 + index, index * 4096, 4096).unwrap())
            .collect();
        aliases[7].begin_mount().unwrap();
        // 整盘查询访问到最后一个分区；测真实 tree/lock CPU 成本，不代表 cache eviction 或设备 I/O。
        crate::verify("block claim raw-read cache policy nine extents", |_| {
            usize::from(black_box(&whole).bypass_cache())
        });
        aliases[7].end_mount();
    }
}

mod cost_policy;
use cost_policy::{ITERATIONS, MAX_NANOSECONDS_PER_OPERATION, SAMPLES};

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

fn verify(name: &str, mut operation: impl FnMut(u64) -> usize) {
    // 1. 先预热同一 release code path，避免首次调度或页错误污染样本。
    black_box(sample(&mut operation));
    // 2. 取五次样本的中位数，单次宿主调度抖动不会直接击穿门禁。
    let elapsed = median(std::array::from_fn(|_| sample(&mut operation)));
    let nanoseconds_per_operation = elapsed.as_nanos() as f64 / ITERATIONS as f64;
    println!("{name}: {nanoseconds_per_operation:.2} ns/op");
    // 3. 该上限远高于当前纯整数实现，但能阻止锁、分配或运行时分派进入热路径。
    assert!(
        nanoseconds_per_operation <= MAX_NANOSECONDS_PER_OPERATION,
        "{name} regressed to {nanoseconds_per_operation:.2} ns/op (limit {MAX_NANOSECONDS_PER_OPERATION:.2} ns/op)"
    );
}

fn benchmark_system_v() {
    use alloc::sync::Arc;
    use ipc::{IpcAccess, IpcNamespace, SemaphoreChange, SemaphoreOperation};
    use syscall_abi::ipc::{GETVAL, IPC_NOWAIT};
    let access = IpcAccess {
        authority: namespace::root_authority(),
        filesystem_gid: 0,
        groups: None,
    };
    let owner = IpcNamespace::create(namespace::initial_user(), 0).unwrap();
    let id = owner.semaphore_get(0, 1, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let view = namespace::initial_pid()
        .child(namespace::initial_user(), 0)
        .unwrap();
    let mut prepared = namespace::PreparedPid::try_new(999, view, &[], &access.authority).unwrap();
    prepared.publish().unwrap();
    let pid = prepared.identity();
    let _registration = prepared.take_registration();
    verify("System V semaphore exact ID lookup", |_| {
        black_box(owner.semaphore_set(black_box(id)).unwrap()).count()
    });
    verify("System V semaphore permission and GETVAL", |_| {
        black_box(&set)
            .scalar(0, GETVAL, black_box(&access), &pid.namespace())
            .unwrap() as usize
    });
    let mut rollback = SemaphoreOperation::new(
        &set,
        vec![SemaphoreChange {
            index: 0,
            delta: -1,
            flags: IPC_NOWAIT as u16,
        }],
        None,
        pid.clone(),
    )
    .unwrap();
    verify(
        "System V semaphore atomic NOWAIT rejection without allocation",
        |tick| {
            usize::from(
                set.operate(black_box(&mut rollback), &access, tick)
                    .unwrap_err()
                    == ipc::IpcError::Again,
            )
        },
    );
    // 实际 operation preparation/completion 是 cold heap 路径，报告 CPU 成本；200ns policy gate 不适用。
    let mut complete = |tick| {
        let mut operation = SemaphoreOperation::new(
            &set,
            vec![
                SemaphoreChange {
                    index: 0,
                    delta: 1,
                    flags: 0,
                },
                SemaphoreChange {
                    index: 0,
                    delta: -1,
                    flags: 0,
                },
            ],
            None,
            Arc::clone(&pid),
        )
        .unwrap();
        usize::from(set.operate(&mut operation, &access, tick).unwrap())
    };
    black_box(sample(&mut complete));
    let elapsed = median(std::array::from_fn(|_| sample(&mut complete)));
    println!(
        "System V semaphore preparation and two-op completion: {:.2} ns/op (diagnostic cold allocation)",
        elapsed.as_nanos() as f64 / ITERATIONS as f64
    );
}

fn main() {
    namespace::boot();
    let restart = spin::Mutex::new(syscall_restart::SyscallRestartState::default());
    verify("actual Thread restart block transaction", |deadline| {
        use syscall_restart::*;
        let mut owner = restart.lock();
        owner.set_block(SyscallRestartBlock::Futex {
            address: 0x2000,
            expected: 7,
            private: true,
            deadline: black_box(deadline),
            bitset: u32::MAX,
        });
        owner.arm(SyscallRestart {
            syscall_id: 98,
            args: [0; 6],
            syscall_pc: 0x1002,
            policy: SyscallRestartPolicy::RestartBlock,
        });
        let number = owner.resolve(None).unwrap().syscall_id;
        black_box(owner.take_block());
        number
    });
    benchmark_system_v();
    fs::benchmark_sysvipc();
    let time = namespace::initial_time();
    verify(
        "time namespace frozen offset snapshot and clock projection",
        |tick| {
            black_box(&time)
                .snapshot()
                .project(namespace::TimeClock::Monotonic, black_box(tick)) as usize
        },
    );
    verify("time namespace absolute deadline conversion", |tick| {
        black_box(namespace::TimeOffsets::ZERO)
            .to_host(namespace::TimeClock::Boottime, black_box(tick)) as usize
    });
    fs::benchmark_block_claim();
    let disabled = seccomp::ThreadSecurityOwner::default();
    verify("seccomp disabled", |iteration| {
        usize::from(
            black_box(&disabled).evaluate(
                172,
                [iteration as usize; 6],
                arch::user::AUDIT_ARCH,
                || 0x1234,
                |_| (),
            ) == seccomp::SeccompDecision::Allow,
        )
    });
    let program = [6, 0, 0, 0, 0, 0, 255, 127]; // Linux RET ALLOW
    let single = seccomp::ThreadSecurityOwner::default();
    model::install_filter(&single, &program);
    let eight = seccomp::ThreadSecurityOwner::default();
    for _ in 0..8 {
        model::install_filter(&eight, &program);
    }
    for (name, security) in [
        ("seccomp one filter", &single),
        ("seccomp eight filters", &eight),
    ] {
        verify(name, |iteration| {
            usize::from(
                black_box(security).evaluate(
                    172,
                    [iteration as usize; 6],
                    arch::user::AUDIT_ARCH,
                    || 0x1234,
                    |_| (),
                ) == seccomp::SeccompDecision::Allow,
            )
        });
    }
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
