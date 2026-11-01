#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production debuglog ring; only CPU/clock, IRQ lock, serial and scheduler waits are host seams.
extern crate alloc;

mod serial {
    #[macro_export]
    macro_rules! println {
        ($($argument:tt)*) => {{ let _ = format_args!($($argument)*); }};
    }
}

mod execution {
    pub(crate) mod cpu {
        pub(crate) struct CpuId;
        impl CpuId {
            pub(crate) fn index(&self) -> usize {
                0
            }
        }
        pub(crate) fn current_id() -> CpuId {
            CpuId
        }
    }
    pub(crate) mod timer {
        pub(crate) fn get_time_us() -> u64 {
            42
        }
    }
    pub(crate) mod deferred {
        #[derive(Clone, Copy)]
        pub(crate) struct DeferredWork;
        pub(crate) fn register(_: fn(u64) -> bool) -> Result<DeferredWork, ()> {
            Ok(DeferredWork)
        }
        pub(crate) fn raise(_: DeferredWork) {}
    }
    pub(crate) mod sync {
        pub(crate) use spin::Mutex as IrqMutex;
        #[derive(Debug, PartialEq, Eq)]
        pub(crate) enum WaitResult {
            Woken,
            TimedOut,
            Interrupted,
            OutOfMemory,
        }
        pub(crate) struct WaitQueue;
        impl WaitQueue {
            pub(crate) const fn new() -> Self {
                Self
            }
            pub(crate) fn wake_all(&self) {}
            pub(crate) fn wait_until(&self, _: impl FnMut() -> bool, _: Option<u64>) -> WaitResult {
                panic!("nonblocking ring test must not enter scheduler wait")
            }
        }
    }
}
#[path = "../../../kernel/src/log/mod.rs"]
mod log;

fn record(sequence: u64) -> alloc::boxed::Box<log::RecordCopy> {
    match log::read_record(sequence, None).unwrap() {
        log::ReadRecord::Record(record) => record,
        _ => panic!("expected live record {sequence}"),
    }
}

#[test]
fn opaque_metadata_survives_ring_read_and_cannot_leak_into_a_reused_native_slot() {
    let annotation = 0x1357_9bdf;
    log::write_external(log::LogLevel::Info, annotation, b"opaque\0bytes");
    let first = record(0);
    assert_eq!(first.annotation, annotation);
    assert_eq!(&first.message[..first.length], b"opaque\0bytes");
    for _ in 1..128 {
        log::write_external(log::LogLevel::Warn, annotation, b"advance ring");
    }
    log::__log(log::LogLevel::Info, "native", format_args!("replacement"));
    assert!(matches!(
        log::read_record(0, None).unwrap(),
        log::ReadRecord::Overrun(1)
    ));
    let replacement = record(128);
    assert_eq!(
        replacement.annotation, 0,
        "native record must clear the prior writer's schema"
    );
    assert_eq!(
        &replacement.message[..replacement.length],
        b"[CPU-0] [native] replacement"
    );
}
