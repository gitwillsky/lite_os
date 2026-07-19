use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use alloc::sync::Arc;

use crate::{
    InodeType,
    drivers::block::{BLOCK_SIZE, BlockDevice, BlockError},
    fs::{
        CreateMetadata, DirectoryEntry, DirectoryVisit, DirectoryVisitor, FileSystem,
        FileSystemError,
        ext2::{
            Ext2FileSystem, TestMappedInode, clear_test_metadata_cache,
            fail_next_test_metadata_owner, reset_test_allocation_attempts,
            reset_test_stage_capacity, reset_test_write_costs, set_test_stage_capacity,
            test_allocation_attempts, test_write_costs,
        },
    },
    regular_write_policy::regular_write_chunk,
    user_iovec::fallible_staging_capacity,
    writeback_batch::REGULAR_WRITE_BATCH_PAGES,
};

pub(crate) static COST_TEST_LOCK: Mutex<()> = Mutex::new(());

struct CountingImage {
    image: Mutex<File>,
    overlay: Mutex<BTreeMap<usize, Vec<u8>>>,
    reads: AtomicUsize,
    writes: AtomicUsize,
    flushes: AtomicUsize,
    fail_next_flush: AtomicBool,
}

impl CountingImage {
    fn open() -> Arc<Self> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fs.img");
        Arc::new(Self {
            image: Mutex::new(File::open(path).expect("open repository ext image")),
            overlay: Mutex::new(BTreeMap::new()),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
            fail_next_flush: AtomicBool::new(false),
        })
    }

    fn reset_reads(&self) {
        self.reads.store(0, Ordering::Relaxed);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn reset_writes(&self) {
        self.writes.store(0, Ordering::Relaxed);
        self.flushes.store(0, Ordering::Relaxed);
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    fn flushes(&self) -> usize {
        self.flushes.load(Ordering::Relaxed)
    }

    fn fail_next_flush(&self) {
        self.fail_next_flush.store(true, Ordering::Relaxed);
    }
}

impl BlockDevice for CountingImage {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        if let Some(block) = self.overlay.lock().unwrap().get(&block_id) {
            buf.copy_from_slice(block);
            return Ok(buf.len());
        }
        let mut image = self.image.lock().unwrap();
        image
            .seek(SeekFrom::Start(block_id as u64 * BLOCK_SIZE as u64))
            .map_err(|_| BlockError::IoError)?;
        image.read_exact(buf).map_err(|_| BlockError::IoError)?;
        Ok(buf.len())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.overlay.lock().unwrap().insert(block_id, buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&self) -> Result<(), BlockError> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        if self.fail_next_flush.swap(false, Ordering::Relaxed) {
            return Err(BlockError::IoError);
        }
        Ok(())
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn dispatch_completions(&self) -> bool {
        false
    }
}

fn mounted() -> (Arc<CountingImage>, Arc<Ext2FileSystem>) {
    let image = CountingImage::open();
    let fs = Ext2FileSystem::new(image.clone()).expect("mount repository ext image");
    (image, fs)
}

struct StopAfterFirst;

impl DirectoryVisitor for StopAfterFirst {
    fn visit(
        &mut self,
        _next_cursor: u64,
        _entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError> {
        Ok(DirectoryVisit::Stop)
    }
}

fn assert_cost(name: &str, reads: usize, allocations: usize) {
    eprintln!("EXT2_COST {name}: device_reads={reads} heap_allocation_attempts={allocations}");
    assert!(reads <= 1, "{name} device read gate: {reads} > 1");
    assert!(
        allocations <= 2,
        "{name} allocation gate: {allocations} > 2"
    );
}

#[test]
fn repeated_lookup_reuses_directory_metadata_block() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let retained = root.find_child(b"bin").unwrap();
    image.reset_reads();
    reset_test_allocation_attempts();
    for _ in 0..16 {
        assert_eq!(
            root.find_child(b"bin").unwrap().metadata().unwrap().inode,
            retained.metadata().unwrap().inode
        );
    }
    assert_cost("lookup_x16", image.reads(), test_allocation_attempts());
}

#[test]
fn repeated_getdents_reuses_directory_metadata_block() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    image.reset_reads();
    reset_test_allocation_attempts();
    for _ in 0..16 {
        root.read_directory(0, &mut StopAfterFirst).unwrap();
    }
    assert_cost("getdents_x16", image.reads(), test_allocation_attempts());
}

#[test]
fn repeated_indirect_mapping_reuses_pointer_metadata_block() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let inode = TestMappedInode::open(fs, &[b"bin", b"busybox"]).unwrap();
    inode.map_repeated(12, 1).unwrap();
    image.reset_reads();
    reset_test_allocation_attempts();
    assert_ne!(inode.map_repeated(12, 16).unwrap(), 0);
    assert_cost("map_block_x16", image.reads(), test_allocation_attempts());
}

#[test]
fn concurrent_warm_lookup_keeps_one_shared_block_identity() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let retained = root.find_child(b"bin").unwrap();
    let inode = retained.metadata().unwrap().inode;
    image.reset_reads();
    reset_test_allocation_attempts();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let root = root.clone();
            scope.spawn(move || {
                for _ in 0..64 {
                    assert_eq!(
                        root.find_child(b"bin").unwrap().metadata().unwrap().inode,
                        inode
                    );
                }
            });
        }
    });
    assert_cost(
        "concurrent_lookup_x256",
        image.reads(),
        test_allocation_attempts(),
    );
}

#[test]
fn committed_rename_publishes_only_the_new_directory_image() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let original = root.find_child(b"bin").unwrap();
    let inode = original.metadata().unwrap().inode;
    root.rename(b"bin", 2, b"bin-cache-rename", true).unwrap();
    assert_eq!(
        root.find_child(b"bin-cache-rename")
            .unwrap()
            .metadata()
            .unwrap()
            .inode,
        inode
    );
    assert!(matches!(
        root.find_child(b"bin"),
        Err(FileSystemError::NotFound)
    ));
}

#[test]
fn truncate_then_reuse_cannot_resurrect_cached_pointer_bytes() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let metadata = CreateMetadata {
        mode: 0o644,
        uid: 0,
        gid: 0,
    };
    let first = root
        .create(b"cache-reuse-first", InodeType::File, metadata)
        .unwrap();
    let offset = 12 * BLOCK_SIZE as u64;
    assert_eq!(first.write_storage(offset, &[0x5a]).unwrap(), 1);
    let first_mapping = TestMappedInode::open(fs.clone(), &[b"cache-reuse-first"])
        .unwrap()
        .map_repeated(12, 2)
        .unwrap();
    first.truncate_storage(0).unwrap();

    let second = root
        .create(b"cache-reuse-second", InodeType::File, metadata)
        .unwrap();
    assert_eq!(second.write_storage(offset, &[0xa5]).unwrap(), 1);
    let second_mapping = TestMappedInode::open(fs, &[b"cache-reuse-second"])
        .unwrap()
        .map_repeated(12, 2)
        .unwrap();
    assert_eq!(
        second_mapping, first_mapping,
        "fixture must exercise physical block reuse"
    );
    let mut byte = [0];
    assert_eq!(second.read_storage(offset, &mut byte).unwrap(), 1);
    assert_eq!(byte, [0xa5]);
}

#[test]
fn cache_owner_oom_never_publishes_a_partial_entry() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    clear_test_metadata_cache(&fs);
    fail_next_test_metadata_owner();
    assert!(matches!(
        root.read_directory(0, &mut StopAfterFirst),
        Err(FileSystemError::OutOfMemory)
    ));
    image.reset_reads();
    root.read_directory(0, &mut StopAfterFirst).unwrap();
    assert_eq!(
        image.reads(),
        1,
        "failed admission must not publish an entry"
    );
    image.reset_reads();
    root.read_directory(0, &mut StopAfterFirst).unwrap();
    assert_eq!(
        image.reads(),
        0,
        "successful retry must populate the only identity"
    );
}

#[test]
fn one_mibibyte_write_has_bounded_transaction_barriers() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"journal-write-cost",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    let input = vec![0x5a; 1024 * 1024];
    let staging_capacity = fallible_staging_capacity(
        input
            .len()
            .min(REGULAR_WRITE_BATCH_PAGES * crate::memory::PAGE_SIZE),
        crate::memory::PAGE_SIZE,
        true,
    );
    image.reset_writes();
    reset_test_write_costs();
    let mut completed = 0usize;
    let mut storage_calls = 0usize;
    while completed < input.len() {
        let count = regular_write_chunk(input.len(), completed, staging_capacity);
        assert_ne!(count, 0, "regular syscall staging made no progress");
        assert_eq!(
            file.write_storage(completed as u64, &input[completed..completed + count])
                .unwrap(),
            count
        );
        completed += count;
        storage_calls += 1;
    }
    let costs = test_write_costs();
    let checkpoint_writes = costs.home_writes - costs.journal_writes;
    eprintln!(
        "EXT2_WRITE_COST write_1MiB: transactions={} flushes={} device_writes={} journal_writes={} checkpoint_writes={}",
        costs.transactions,
        image.flushes(),
        image.writes(),
        costs.journal_writes,
        checkpoint_writes
    );
    assert_eq!(
        storage_calls, 1,
        "1 MiB syscall staging split storage calls"
    );
    assert_eq!(costs.transactions, 1);
    assert!(
        image.flushes() <= 3,
        "one transaction exceeded the three journal barriers"
    );
}

#[test]
fn truncate_batches_allocation_metadata_for_fixed_block_count() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"allocation-free-cost",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    const BLOCKS: usize = 64;
    file.allocate_storage(0, (BLOCKS * BLOCK_SIZE) as u64)
        .unwrap();
    image.reset_writes();
    reset_test_allocation_attempts();
    reset_test_write_costs();
    file.truncate_storage(0).unwrap();
    let costs = test_write_costs();
    let checkpoint_writes = costs.home_writes - costs.journal_writes;
    eprintln!(
        "EXT2_WRITE_COST free_{BLOCKS}: transactions={} flushes={} device_writes={} journal_writes={} checkpoint_writes={} allocation_materializations={} metadata_prepare_bytes={} allocation_attempts={}",
        costs.transactions,
        image.flushes(),
        image.writes(),
        costs.journal_writes,
        checkpoint_writes,
        costs.allocation_materializations,
        costs.allocation_metadata_bytes,
        test_allocation_attempts()
    );
    assert_eq!(costs.transactions, 1);
    assert!(
        costs.allocation_materializations <= 1,
        "allocation metadata synchronized per freed block"
    );
    assert!(
        costs.allocation_metadata_bytes <= 32 * 1024,
        "allocation metadata rebuilt more than one bounded dirty batch"
    );
}

#[test]
fn failed_commit_rolls_back_dirty_allocation_and_recovery_ignores_it() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"commit-failure-recovery",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    image.fail_next_flush();
    assert!(matches!(
        file.write_storage(0, &[0x77]),
        Err(FileSystemError::IoError)
    ));
    assert_eq!(
        file.size(),
        0,
        "live inode must roll back after failed commit"
    );
    drop(file);
    drop(root);
    drop(fs);

    let recovered = Ext2FileSystem::new(image).expect("remount after uncommitted journal write");
    let recovered_file = recovered
        .root_inode()
        .unwrap()
        .find_child(b"commit-failure-recovery")
        .unwrap();
    assert_eq!(recovered_file.size(), 0);
}

#[test]
fn journal_enospc_aborts_dirty_owner_without_partial_namespace() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let metadata = CreateMetadata {
        mode: 0o644,
        uid: 0,
        gid: 0,
    };
    set_test_stage_capacity(1);
    let result = root.create(b"journal-enospc", InodeType::File, metadata);
    reset_test_stage_capacity();
    assert!(matches!(result, Err(FileSystemError::NoSpace)));
    assert!(matches!(
        root.find_child(b"journal-enospc"),
        Err(FileSystemError::NotFound)
    ));
    root.create(b"journal-enospc", InodeType::File, metadata)
        .expect("aborted capacity failure must leave journal reusable");
}

#[test]
fn concurrent_truncate_and_indirect_write_publish_one_serial_order() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"concurrent-truncate",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    let offset = 12 * BLOCK_SIZE as u64;
    file.write_storage(offset, &[0x11]).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let truncate_file = file.clone();
        let truncate_barrier = barrier.clone();
        scope.spawn(move || {
            truncate_barrier.wait();
            truncate_file.truncate_storage(0).unwrap();
        });
        let write_file = file.clone();
        let write_barrier = barrier.clone();
        scope.spawn(move || {
            write_barrier.wait();
            write_file.write_storage(offset, &[0x66]).unwrap();
        });
    });
    match file.size() {
        0 => {}
        size => {
            assert_eq!(size, offset + 1);
            let mut byte = [0];
            assert_eq!(file.read_storage(offset, &mut byte).unwrap(), 1);
            assert_eq!(byte, [0x66]);
        }
    }
    file.write_storage(0, &[0x7f]).unwrap();
}
