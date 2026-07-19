use super::*;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
