#![feature(allocator_ext)]
extern crate alloc;
macro_rules! error { ($($message:tt)*) => { std::eprintln!($($message)*) }; }
macro_rules! info { ($($message:tt)*) => { std::eprintln!($($message)*) }; }
// Native events are host notification inputs; all protocol/queue/lifetime decisions execute netstack.
extern crate self as native_runtime;
pub use native_abi;
#[path = "../../kernel-unit/src/sync.rs"]
#[expect(
    dead_code,
    reason = "partial host scheduler seam; IRQ, deadline and readiness owners are tested by their own fixtures"
)]
mod sync;
/// 安装协议交出的接口 lifetime channel；host 夹具只经初始网卡装配，不构造它。
pub(crate) struct Channel;
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
#[path = "../../../system/netstack/src/identity.rs"]
mod identity;
#[path = "../../../system/netstack/src/namespace.rs"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "RPC namespace dispatch is exercised in the guest, this assembly drives protocol lifetimes directly"
    )
)]
mod namespace;
#[path = "../../../system/netstack/src/packet.rs"]
#[expect(
    dead_code,
    reason = "host assembly omits RPC dispatch and exercises packet queue owners directly"
)]
mod packet;
#[path = "../../../system/netstack/src/receive.rs"]
mod receive;
#[path = "../../../tools/source-view/netstack/inet/inet/inet.rs"]
#[expect(
    dead_code,
    unused_imports,
    reason = "partial host protocol assembly; production RPC server consumes the remaining exports"
)]
mod inet {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/source-view/netstack/inet/inet/inet.rs"
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
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/configuration_tests.rs"
            ));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/routing_compilation_tests.rs"
            ));
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/packet_tests.rs"));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/rx_batch_tests.rs"
            ));
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

/// Measure actual netstack protocol owners with host NIC and event boundaries.
/// `measure` receives name, iteration count, logical handle count, gate status, and operation.
pub fn benchmark_lifecycle(mut measure: impl FnMut(&str, u64, usize, bool, &mut dyn FnMut())) {
    inet::fixture::benchmark(&mut measure);
}

#[cfg(test)]
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;

#[cfg(test)]
#[path = "../../../system/router-config/src/transaction/storage.rs"]
mod configuration_storage;
#[cfg(test)]
#[path = "../../../system/router-config/src/policy.rs"]
mod configuration_policy;
#[cfg(test)]
#[path = "../../../system/router-config/src/backup/archive.rs"]
mod backup_archive;
#[cfg(test)]
mod configuration_storage_tests;
#[cfg(test)]
mod backup_archive_tests;
