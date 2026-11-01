    #[expect(dead_code, reason = "the fixture tests topology and scheduling without hardware startup")]
    #[path = "../../../kernel/src/execution/cpu/mod.rs"]
    pub(crate) mod cpu;
    pub(crate) mod sync {
        pub(crate) type IrqMutex<T> = spin::Mutex<T>;
        #[derive(Debug, Copy, Clone, PartialEq, Eq)]
        pub(crate) struct TaskWaitKey;
        #[derive(Debug, Copy, Clone, PartialEq, Eq)]
        pub(crate) enum WaitResult {}
    }
