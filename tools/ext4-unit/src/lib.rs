#![cfg_attr(test, feature(allocator_ext))]

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
macro_rules! error { ($($argument:tt)*) => { std::eprintln!($($argument)*) }; }

#[cfg(test)]
#[path = "../../kernel-unit/src/sync.rs"]
#[expect(dead_code, reason = "filesystem assembly uses sleeping locks; readiness and service-thread entry points have separate fixtures")]
mod sync;

#[cfg(test)]
#[path = "../../../system/ext4d/src/vfs.rs"]
#[expect(dead_code, reason = "FUSE encoding methods are consumed by the guest service; this fixture tests the ext4 engine")]
mod vfs;
#[cfg(test)]
use vfs::InodeType;

#[cfg(test)]
#[path = "../../../tools/source-view/ext4d/ext4/ext4/ext4.rs"]
#[expect(dead_code, reason = "the host fixture mounts a memory-backed device; service mount and pager entry points run in the guest")]
mod ext4;

#[cfg(test)]
mod time {
    pub(crate) fn get_realtime_ns() -> u64 { 1_800_000_000_000_000_000 }
    pub(crate) fn get_time_ns() -> u64 { 1_000_000_000 }
}

#[cfg(test)]
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;

#[cfg(test)]
mod ext4_cost_tests;
#[cfg(test)]
mod ext4_recovery_tests;
#[cfg(test)]
mod ext4_conformance_tests;
