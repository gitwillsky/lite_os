#![feature(allocator_ext)]
extern crate alloc;
macro_rules! error { ($($message:tt)*) => { std::eprintln!($($message)*) }; }
// Native events are host notification inputs; all protocol/queue/lifetime decisions execute netd.
extern crate self as native_runtime;
pub use native_abi;
#[path = "../../kernel-unit/src/sync.rs"]
#[expect(
    dead_code,
    reason = "partial host scheduler seam; IRQ, deadline and readiness owners are tested by their own fixtures"
)]
mod sync;
#[derive(Default)]
pub(crate) struct Event(core::sync::atomic::AtomicU32);
impl Event {
    pub(crate) fn signal(&self, clear: u32, set: u32) -> Result<(), native_abi::Status> {
        let _ = self.0.try_update(
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
            |value| Some(value & !clear | set),
        );
        Ok(())
    }
}
mod time {
    pub(crate) fn get_time_us() -> u64 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_micros() as u64
    }
}
mod worker {
    pub(crate) fn request_poll() {}
}
#[path = "../../../system/netd/src/identity.rs"]
mod identity;
#[path = "../../../system/netd/src/namespace.rs"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "RPC namespace dispatch is exercised in the guest, this assembly drives protocol lifetimes directly"
    )
)]
mod namespace;
#[path = "../../../system/netd/src/packet.rs"]
#[expect(
    dead_code,
    reason = "host assembly omits RPC dispatch and exercises packet queue owners directly"
)]
mod packet;
#[path = "../../../system/netd/src/receive.rs"]
mod receive;
#[path = "../../../tools/source-view/netd/inet/inet/inet.rs"]
#[expect(
    dead_code,
    unused_imports,
    reason = "partial host protocol assembly; production RPC server consumes the remaining exports"
)]
mod inet {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/source-view/netd/inet/inet/inet.rs"
    ));
    pub(crate) mod fixture {
        use super::*;
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/wire.rs"));
        mod benchmark {
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/benchmark.rs"));
        }
        pub(crate) use benchmark::run as benchmark;
        #[cfg(test)]
        mod tests {
            use super::*;
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/udp_tests.rs"));
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tcp_tests.rs"));
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/domain_tests.rs"));
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/routing_tests.rs"));
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/packet_tests.rs"));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/namespace_tests.rs"
            ));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/packet_domain_tests.rs"
            ));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/tcp_timer_tests.rs"
            ));
        }
    }
}

/// Measure actual netd protocol owners with host NIC and event boundaries.
/// `measure` receives name, iteration count, logical handle count, gate status, and operation.
pub fn benchmark_lifecycle(mut measure: impl FnMut(&str, u64, usize, bool, &mut dyn FnMut())) {
    inet::fixture::benchmark(&mut measure);
}

#[cfg(test)]
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;
