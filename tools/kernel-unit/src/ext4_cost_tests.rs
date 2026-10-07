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

use alloc::{format, sync::Arc};

use crate::{
    InodeType,
    drivers::block::{BLOCK_SIZE, BlockDevice, BlockError},
    fs::{
        CreateMetadata, DirectoryEntry, DirectoryVisit, DirectoryVisitor, FileSystem,
        FileSystemError,
        ext4::{
            Ext4FileSystem, TestMappedInode, clear_test_metadata_cache,
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

/// 返回 `scripts/workflow.py` 的 `verify_unit` 生成的只读 ext4 fixture 路径。
///
/// fixture 由 `create_fs.py` 的唯一 ext4 layout 生成：4K block、JBD2 journal、`/bin` 与跨越
/// direct block 的 `/bin/init`。测试写入只进入内存 overlay，不修改 fixture。
///
/// # Panics
///
/// 未设置 `LITEOS_EXT4_FIXTURE` 时 panic；直接 `cargo test` 必须改用 `make verify-unit`。
pub(crate) fn ext4_fixture_path() -> PathBuf {
    std::env::var_os("LITEOS_EXT4_FIXTURE")
        .map(PathBuf::from)
        .expect("LITEOS_EXT4_FIXTURE is unset; run `make verify-unit` to generate the ext4 fixture")
}

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
        Arc::new(Self {
            image: Mutex::new(File::open(ext4_fixture_path()).expect("open ext4 fixture image")),
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
    fn disk_name(&self) -> &[u8] {
        b"vda"
    }

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
}

fn mounted() -> (Arc<CountingImage>, Arc<Ext4FileSystem>) {
    let image = CountingImage::open();
    let fs = Ext4FileSystem::new(image.clone()).expect("mount ext4 fixture image");
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
    eprintln!("EXT4_COST {name}: device_reads={reads} heap_allocation_attempts={allocations}");
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
fn repeated_extent_mapping_reuses_metadata_blocks() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let inode = TestMappedInode::open(fs, &[b"bin", b"init"]).unwrap();
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
fn truncate_then_reuse_cannot_resurrect_cached_block_bytes() {
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

    // 1. data=ordered：未提交事务释放的 block 在提交前不得重新分配，否则崩溃后旧文件会指向
    //    新文件已写回 home 的数据。
    let second = root
        .create(b"cache-reuse-second", InodeType::File, metadata)
        .unwrap();
    assert_eq!(second.write_storage(offset, &[0x3c]).unwrap(), 1);
    let second_mapping = TestMappedInode::open(fs.clone(), &[b"cache-reuse-second"])
        .unwrap()
        .map_repeated(12, 2)
        .unwrap();
    assert_ne!(
        second_mapping, first_mapping,
        "uncommitted free must not be reallocated"
    );

    // 2. 提交后释放的 block 可复用，且新 owner 读到的是自己的数据而不是缓存中的旧 image。
    root.sync_storage().unwrap();
    let third = root
        .create(b"cache-reuse-third", InodeType::File, metadata)
        .unwrap();
    assert_eq!(third.write_storage(offset, &[0xa5]).unwrap(), 1);
    let third_mapping = TestMappedInode::open(fs, &[b"cache-reuse-third"])
        .unwrap()
        .map_repeated(12, 2)
        .unwrap();
    assert_eq!(
        third_mapping, first_mapping,
        "fixture must exercise physical block reuse after commit"
    );
    let mut byte = [0];
    assert_eq!(third.read_storage(offset, &mut byte).unwrap(), 1);
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
    root.sync_storage().unwrap();
    let costs = test_write_costs();
    let checkpoint_writes = costs.home_writes - costs.journal_writes;
    eprintln!(
        "EXT4_WRITE_COST write_1MiB: transactions={} flushes={} device_writes={} journal_writes={} checkpoint_writes={}",
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
    root.sync_storage().unwrap();
    let costs = test_write_costs();
    let checkpoint_writes = costs.home_writes - costs.journal_writes;
    eprintln!(
        "EXT4_WRITE_COST free_{BLOCKS}: transactions={} flushes={} device_writes={} journal_writes={} checkpoint_writes={} allocation_materializations={} metadata_prepare_bytes={} allocation_attempts={}",
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
fn failed_commit_fails_stop_and_recovery_ignores_it() {
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
    file.write_storage(0, &[0x77]).unwrap();
    // commit record 前的 barrier 失败：sync 必须报告 EIO，journal 进入 fail-stop。
    image.fail_next_flush();
    assert!(matches!(root.sync_storage(), Err(FileSystemError::IoError)));
    assert!(
        matches!(
            file.write_storage(1, &[0x78]),
            Err(FileSystemError::IoError)
        ),
        "mutation after a failed commit must not reach a second write path"
    );
    drop(file);
    drop(root);
    drop(fs);

    // 未写出 commit record 的事务在 replay 时整体丢弃。
    let recovered = Ext4FileSystem::new(image).expect("remount after failed commit");
    assert!(matches!(
        recovered
            .root_inode()
            .unwrap()
            .find_child(b"commit-failure-recovery"),
        Err(FileSystemError::NotFound)
    ));
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
fn concurrent_truncate_and_sparse_write_publish_one_serial_order() {
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

/// 一类写负载在 `sync` 后的确定性设备成本。
#[derive(Debug, Clone, Copy)]
struct WorkloadCost {
    transactions: usize,
    device_writes: usize,
    flushes: usize,
}

fn measure_workload(name: &str, workload: impl FnOnce(&Arc<dyn crate::fs::Inode>)) -> WorkloadCost {
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    image.reset_writes();
    reset_test_write_costs();
    workload(&root);
    root.sync_storage().unwrap();
    let cost = WorkloadCost {
        transactions: test_write_costs().transactions,
        device_writes: image.writes(),
        flushes: image.flushes(),
    };
    eprintln!(
        "EXT4_WORKLOAD {name}: transactions={} device_writes={} flushes={}",
        cost.transactions, cost.device_writes, cost.flushes
    );
    cost
}

const FILE_METADATA: CreateMetadata = CreateMetadata {
    mode: 0o644,
    uid: 0,
    gid: 0,
};

/// 写路径的设备成本门禁：小文件批量创建、同一文件小块追加、大文件顺序写。
#[test]
fn write_workloads_have_bounded_device_cost() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    // 1. 100 个 4 KiB 小文件：每个文件一次 create + 一次 write。
    let small_files = measure_workload("small_files_100x4k", |root| {
        for index in 0..100 {
            let file = root
                .create(
                    format!("small-{index:03}").as_bytes(),
                    InodeType::File,
                    FILE_METADATA,
                )
                .unwrap();
            file.write_storage(0, &[index as u8; BLOCK_SIZE]).unwrap();
        }
    });
    // 2. 同一文件 256 次 4 KiB 追加（日志、下载流式落盘）。
    let appends = measure_workload("append_256x4k", |root| {
        let file = root
            .create(b"append", InodeType::File, FILE_METADATA)
            .unwrap();
        for index in 0..256u64 {
            file.write_storage(index * BLOCK_SIZE as u64, &[index as u8; BLOCK_SIZE])
                .unwrap();
        }
    });
    // 3. 4 MiB 顺序写，每次 64 KiB。
    let sequential = measure_workload("sequential_4m_64k", |root| {
        let file = root
            .create(b"sequential", InodeType::File, FILE_METADATA)
            .unwrap();
        let chunk = [0x5a; 64 * 1024];
        for index in 0..64u64 {
            file.write_storage(index * chunk.len() as u64, &chunk)
                .unwrap();
        }
    });
    // data=ordered + group commit：每类负载在 sync 时只形成一个事务、三个 barrier；数据只写
    // 一次 home，设备写入只比数据 block 数多出有界的 metadata/journal 开销。改造前三类负载
    // 分别为 200/257/65 个事务、601/772/196 次 flush，数据经 journal 写两次。
    for (name, cost, data_blocks) in [
        ("small_files_100x4k", small_files, 100),
        ("append_256x4k", appends, 256),
        ("sequential_4m_64k", sequential, 1024),
    ] {
        assert_eq!(cost.transactions, 1, "{name}: {cost:?}");
        assert!(cost.flushes <= 3, "{name}: {cost:?}");
        assert!(
            cost.device_writes <= data_blocks + 64,
            "{name}: data written more than once or unbounded metadata: {cost:?}"
        );
    }
}
