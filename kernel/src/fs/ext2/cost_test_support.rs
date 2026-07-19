use super::*;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

// OWNER: serialized host cost tests own this diagnostic counter; production does not compile it.
#[cfg(test)]
static ALLOCATION_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
// OWNER: one host test consumes this single admission fault before another test can publish it.
#[cfg(test)]
static FAIL_NEXT_METADATA_OWNER: AtomicBool = AtomicBool::new(false);
// OWNER: serialized host write-cost tests own these deterministic event counters.
#[cfg(test)]
static TRANSACTIONS: AtomicUsize = AtomicUsize::new(0);
// OWNER: serialized host write-cost tests own the journal-write counter.
#[cfg(test)]
static JOURNAL_WRITES: AtomicUsize = AtomicUsize::new(0);
// OWNER: serialized host write-cost tests own the home-write counter.
#[cfg(test)]
static HOME_WRITES: AtomicUsize = AtomicUsize::new(0);
// OWNER: serialized host write-cost tests own the allocation-metadata materialization counter.
#[cfg(test)]
static ALLOCATION_MATERIALIZATIONS: AtomicUsize = AtomicUsize::new(0);
// OWNER: serialized host write-cost tests own the metadata-byte counter.
#[cfg(test)]
static ALLOCATION_METADATA_BYTES: AtomicUsize = AtomicUsize::new(0);
// OWNER: serialized host ENOSPC test owns this journal capacity override.
#[cfg(test)]
static STAGE_CAPACITY: AtomicUsize = AtomicUsize::new(usize::MAX);
// OWNER: one serialized host recovery test owns this admission/release rendezvous. It forces the
// stale-successor interleaving; without either edge the concurrency regression is nondeterministic.
#[cfg(test)]
static ORPHAN_DROP_TARGET: AtomicU32 = AtomicU32::new(0);
// OWNER: the same serialized recovery test publishes this one-shot admission edge.
#[cfg(test)]
static ORPHAN_DROP_ADMITTED: AtomicBool = AtomicBool::new(false);
// OWNER: the same serialized recovery test publishes this one-shot release edge.
#[cfg(test)]
static ORPHAN_DROP_RELEASED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct TestWriteCosts {
    pub(crate) transactions: usize,
    pub(crate) journal_writes: usize,
    pub(crate) home_writes: usize,
    pub(crate) allocation_materializations: usize,
    pub(crate) allocation_metadata_bytes: usize,
}

#[cfg(test)]
pub(super) fn record_test_allocation_attempt() {
    ALLOCATION_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn reset_test_allocation_attempts() {
    ALLOCATION_ATTEMPTS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn test_allocation_attempts() -> usize {
    ALLOCATION_ATTEMPTS.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn reset_test_write_costs() {
    TRANSACTIONS.store(0, Ordering::Relaxed);
    JOURNAL_WRITES.store(0, Ordering::Relaxed);
    HOME_WRITES.store(0, Ordering::Relaxed);
    ALLOCATION_MATERIALIZATIONS.store(0, Ordering::Relaxed);
    ALLOCATION_METADATA_BYTES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn test_write_costs() -> TestWriteCosts {
    TestWriteCosts {
        transactions: TRANSACTIONS.load(Ordering::Relaxed),
        journal_writes: JOURNAL_WRITES.load(Ordering::Relaxed),
        home_writes: HOME_WRITES.load(Ordering::Relaxed),
        allocation_materializations: ALLOCATION_MATERIALIZATIONS.load(Ordering::Relaxed),
        allocation_metadata_bytes: ALLOCATION_METADATA_BYTES.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
pub(crate) fn set_test_stage_capacity(capacity: usize) {
    STAGE_CAPACITY.store(capacity, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn reset_test_stage_capacity() {
    STAGE_CAPACITY.store(usize::MAX, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn test_stage_capacity(capacity: usize) -> usize {
    capacity.min(STAGE_CAPACITY.load(Ordering::Relaxed))
}

#[cfg(test)]
pub(super) fn record_test_transaction() {
    TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn record_test_journal_write() {
    JOURNAL_WRITES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn record_test_home_write() {
    HOME_WRITES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn record_test_allocation_materialization() {
    ALLOCATION_MATERIALIZATIONS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn record_test_allocation_metadata_bytes(bytes: usize) {
    ALLOCATION_METADATA_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn fail_next_test_metadata_owner() {
    FAIL_NEXT_METADATA_OWNER.store(true, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn fail_test_metadata_owner() -> bool {
    FAIL_NEXT_METADATA_OWNER.swap(false, Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn clear_test_metadata_cache(fs: &Ext2FileSystem) {
    fs.metadata_cache.lock().clear();
}

#[cfg(test)]
pub(crate) fn test_mount_allocation_state(fs: &Ext2FileSystem) -> (u32, u32, u32) {
    let superblock = fs.superblock.lock();
    (
        superblock.s_free_blocks_count,
        superblock.s_free_inodes_count,
        superblock.s_last_orphan,
    )
}

#[cfg(test)]
pub(crate) fn arm_test_orphan_drop(inode: u32) {
    assert_ne!(inode, 0);
    ORPHAN_DROP_ADMITTED.store(false, Ordering::Relaxed);
    ORPHAN_DROP_RELEASED.store(false, Ordering::Relaxed);
    ORPHAN_DROP_TARGET.store(inode, Ordering::Release);
}

#[cfg(test)]
pub(super) fn test_orphan_drop_admission(inode: u32) {
    if ORPHAN_DROP_TARGET.load(Ordering::Acquire) != inode {
        return;
    }
    ORPHAN_DROP_ADMITTED.store(true, Ordering::Release);
    while !ORPHAN_DROP_RELEASED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
pub(crate) fn wait_test_orphan_drop_admission() {
    while !ORPHAN_DROP_ADMITTED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
pub(crate) fn release_test_orphan_drop() {
    ORPHAN_DROP_RELEASED.store(true, Ordering::Release);
    ORPHAN_DROP_TARGET.store(0, Ordering::Release);
}

#[cfg(test)]
pub(crate) struct TestMappedInode(Arc<Ext2Inode>);

#[cfg(test)]
impl TestMappedInode {
    pub(crate) fn open(
        fs: Arc<Ext2FileSystem>,
        components: &[&[u8]],
    ) -> Result<Self, FileSystemError> {
        let mut inode = Ext2Inode::load(fs, 2)?;
        for component in components {
            let child = inode.find_child(component)?;
            let number = child.metadata()?.inode as u32;
            drop(child);
            inode = Ext2Inode::load(inode.fs.clone(), number)?;
        }
        Ok(Self(inode))
    }

    pub(crate) fn map_repeated(
        &self,
        file_block_index: u32,
        repetitions: usize,
    ) -> Result<u32, FileSystemError> {
        let mut block = 0;
        for _ in 0..repetitions {
            block = self.0.map_block(file_block_index)?;
        }
        Ok(block)
    }
}
