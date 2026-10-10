#![feature(allocator_ext)]
#![allow(dead_code)]
//! 生产 kernel PID owner 与执行身份投影；仅装配未调度的执行体输入。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
mod fs {
    pub(crate) mod pidfs {
        #[derive(Default)]
        pub(crate) struct PidFdSource(pub(crate) core::sync::atomic::AtomicUsize);
        impl PidFdSource {
            pub(crate) fn notify(&self) {
                self.0.fetch_add(1, core::sync::atomic::Ordering::Release);
            }
        }
    }
}
#[path = "../../../kernel/src/task"]
mod task {
    #[path = "pid.rs"]
    #[allow(
        unused_imports,
        reason = "focused identity fixture consumes a subset of the actual PID facade"
    )]
    pub(crate) mod pid;
    #[path = "model"]
    mod model {
        use crate::{namespace, task::pid::ProcessId};
        use alloc::sync::Arc;
        use spin::Mutex;
        // Credential 值为此 identity-only fixture 的构造边界；实际 root credential 另由 proc guest 验证。
        struct Credentials;
        impl Credentials {
            fn root() -> Self {
                Self
            }
        }
        type KernelThreadBody = alloc::boxed::Box<dyn FnOnce() + Send>;
        // namespace tuple 为构造边界；此 identity-only fixture 传入一个实际 PID namespace Arc，
        // 退出/observer/pin 机制执行 production KernelThread，完整 tuple 的选择由 proc source 围栏覆盖。
        mod namespaces {
            pub(super) type NamespaceMembership = alloc::sync::Arc<crate::namespace::PidNamespace>;
        }
        #[path = "kernel_thread.rs"]
        mod kernel_thread;
        use kernel_thread::KernelThread;
        #[path = "thread_identity.rs"]
        mod thread_identity;
        struct Process {
            tgid: ProcessId,
        }
        enum TaskKind {
            User { process: Arc<Process> },
            Kernel(KernelThread),
        }
        struct ExecutionContext {
            identity: Mutex<Arc<namespace::PidIdentity>>,
        }
        struct TaskControlBlock {
            kind: TaskKind,
            execution: ExecutionContext,
        }
        impl TaskControlBlock {
            fn is_kernel_thread(&self) -> bool {
                matches!(self.kind, TaskKind::Kernel(_))
            }
            fn process(&self) -> &Arc<Process> {
                let TaskKind::User { process } = &self.kind else {
                    panic!("kernel has no user Process")
                };
                process
            }
        }
        #[test]
        fn kernel_current_actor_has_real_pid_and_exit_releases_it_before_observers_drop() {
            namespace::boot();
            let root = namespace::initial_pid();
            let mut pid = namespace::PreparedPid::try_new(
                71_001,
                root.clone(),
                &[],
                &namespace::root_authority(),
            )
            .unwrap();
            let observer = pid.identity();
            let kernel = KernelThread::new(
                pid.take_registration(),
                "retire-mm",
                alloc::boxed::Box::new(|| {}),
                root.clone(),
            );
            let task = Arc::new(TaskControlBlock {
                kind: TaskKind::Kernel(kernel),
                execution: ExecutionContext {
                    identity: Mutex::new(observer.clone()),
                },
            });
            pid.publish().unwrap();
            drop(pid);
            let number = observer.number_in(&root);
            assert_ne!(number, 0);
            assert_eq!(task.visible_tid(), number);
            assert_eq!(task.visible_tgid(), number);
            assert_eq!(task.tid(), observer.key());
            assert_eq!(task.tgid(), observer.key());
            assert!(Arc::ptr_eq(task.tgid_identity(), &observer));
            assert!(Arc::ptr_eq(&task.tid_identity(), &observer));
            assert!(Arc::ptr_eq(&task.pid_namespace(), &root));
            assert_eq!(task.resolve_pid(number as usize), Some(observer.key()));
            assert!(!task.is_namespace_init());
            let TaskKind::Kernel(kernel) = &task.kind else {
                unreachable!()
            };
            kernel.take_body()();
            assert!(kernel.namespaces.lock().is_some());
            kernel.retire_pid();
            assert!(kernel.namespaces.lock().is_none());
            assert_eq!(root.lookup(number), None);
            assert_eq!(
                task.visible_tgid(),
                number,
                "historical pin keeps projection, not occupancy"
            );
            assert_eq!(observer.number_in(&root), number);
        }
        #[test]
        fn failed_unpublished_kernel_task_returns_reservation_without_publishing_a_pid() {
            namespace::boot();
            let root = namespace::initial_pid();
            let mut pid = namespace::PreparedPid::try_new(
                71_002,
                root.clone(),
                &[],
                &namespace::root_authority(),
            )
            .unwrap();
            let observer = pid.identity();
            let kernel = KernelThread::new(
                pid.take_registration(),
                "unpublished",
                alloc::boxed::Box::new(|| {}),
                root.clone(),
            );
            assert_eq!(root.lookup(observer.number_in(&root)), None);
            drop(kernel);
            drop(pid);
            assert_eq!(root.lookup(observer.number_in(&root)), None);
            assert_eq!(observer.key(), 71_002);
        }
    }
}
