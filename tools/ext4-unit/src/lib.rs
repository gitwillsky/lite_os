#![cfg_attr(test, feature(allocator_ext))]

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
macro_rules! error { ($($argument:tt)*) => { std::eprintln!($($argument)*) }; }

#[cfg(test)]
#[path = "../../kernel-unit/src/sync.rs"]
mod sync;

#[cfg(test)]
#[path = "../../../system/ext4d/src/vfs.rs"]
mod vfs;
#[cfg(test)]
use vfs::InodeType;

#[cfg(test)]
#[path = "../../../tools/source-view/ext4d/ext4/ext4/ext4.rs"]
mod ext4;

#[cfg(test)]
mod time {
    pub(crate) fn get_realtime_ns() -> u64 { 1_800_000_000_000_000_000 }
    pub(crate) fn get_time_ns() -> u64 { 1_000_000_000 }
}

#[cfg(test)]
#[path = "../../../system/linuxd/src/syscall/fs/io/write_limit.rs"]
mod regular_write_policy;
#[cfg(test)]
#[path = "../../../system/linuxd/src/syscall/user_iovec/input_staging.rs"]
mod user_iovec;
#[cfg(test)]
#[path = "../../../system/linuxd/src/fs/page_cache/writeback_batch.rs"]
mod writeback_batch;

#[cfg(test)]
mod ext4_cost_tests;
#[cfg(test)]
mod ext4_recovery_tests;
#[cfg(test)]
mod ext4_conformance_tests;
