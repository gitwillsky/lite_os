//! 执行实际 deferred owner；仅替换 logical CPU、software interrupt 与平台 IPI 边界。
extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};

static LOCAL_EDGES: AtomicUsize = AtomicUsize::new(0);
static REMOTE_EDGES: AtomicUsize = AtomicUsize::new(0);
static REMOTE_MASK: AtomicUsize = AtomicUsize::new(0);

mod execution {
    pub(crate) mod cpu {
        use core::cell::Cell;
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) struct CpuId(pub(crate) usize);
        impl CpuId {
            pub(crate) fn index(self) -> usize {
                self.0
            }
        }
        pub(crate) struct CpuSet(pub(crate) usize);
        impl CpuSet {
            pub(crate) fn singleton(cpu: CpuId) -> Self {
                Self(1 << cpu.0)
            }
        }
        std::thread_local! { pub(crate) static CURRENT: Cell<CpuId> = const { Cell::new(CpuId(0)) }; }
        pub(crate) fn current_id() -> CpuId {
            CURRENT.with(Cell::get)
        }
    }
}
mod arch {
    pub(crate) mod interrupt {
        pub(crate) fn raise_software() {
            crate::LOCAL_EDGES.fetch_add(1, crate::Ordering::Relaxed);
        }
    }
}
mod platform {
    pub(crate) fn send_ipi(cpus: crate::execution::cpu::CpuSet) -> Result<(), ()> {
        crate::REMOTE_EDGES.fetch_add(1, crate::Ordering::Relaxed);
        crate::REMOTE_MASK.fetch_or(cpus.0, crate::Ordering::Relaxed);
        Ok(())
    }
}
#[expect(
    dead_code,
    reason = "this fixture exercises core work publication; registered device handlers are outside its contract"
)]
#[path = "../../../kernel/src/execution/deferred.rs"]
mod deferred;

#[test]
fn remote_work_is_owned_by_target_and_reissues_an_edge_after_take() {
    use deferred::DeferredWork;
    use execution::cpu::{CURRENT, CpuId};
    deferred::initialize(2);
    deferred::raise_on(CpuId(1), DeferredWork::RUNQUEUE);
    deferred::raise_on(CpuId(1), DeferredWork::RUNQUEUE);
    deferred::raise_on(CpuId(1), DeferredWork::CONSOLE);
    assert_eq!(REMOTE_EDGES.load(Ordering::Relaxed), 1);
    assert_eq!(REMOTE_MASK.load(Ordering::Relaxed), 2);
    assert_eq!(LOCAL_EDGES.load(Ordering::Relaxed), 0);
    assert!(
        deferred::take().is_empty(),
        "sender cannot consume the target's work"
    );
    CURRENT.with(|cpu| cpu.set(CpuId(1)));
    let work = deferred::take();
    assert!(work.contains(DeferredWork::RUNQUEUE));
    assert!(work.contains(DeferredWork::CONSOLE));
    assert!(deferred::take().is_empty());
    // 接管后的新发布必须有新 edge，不能被上一批合并状态吞掉。
    CURRENT.with(|cpu| cpu.set(CpuId(0)));
    deferred::raise_on(CpuId(1), DeferredWork::RUNQUEUE);
    assert_eq!(REMOTE_EDGES.load(Ordering::Relaxed), 2);
    CURRENT.with(|cpu| cpu.set(CpuId(1)));
    assert!(deferred::take().contains(DeferredWork::RUNQUEUE));
    deferred::raise(DeferredWork::CONSOLE);
    assert_eq!(LOCAL_EDGES.load(Ordering::Relaxed), 1);
    assert!(deferred::take().contains(DeferredWork::CONSOLE));
}
