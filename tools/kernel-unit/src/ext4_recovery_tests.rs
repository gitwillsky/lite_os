use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use alloc::sync::Arc;

use crate::ext4_cost_tests::{COST_TEST_LOCK, ext4_fixture_path};
use crate::{
    InodeType,
    drivers::block::{BLOCK_SIZE, BlockDevice, BlockError},
    fs::{
        CreateMetadata, FileSystem, FileSystemError,
        ext4::{
            Ext4FileSystem, arm_test_orphan_drop, release_test_orphan_drop,
            test_mount_allocation_state, test_orphan_drop_admitted,
            wait_test_orphan_drop_admission, with_test_mutation_lock,
        },
    },
};

const JBD2_MAGIC: u32 = 0xC03B_3998;
const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
const JBD2_COMMIT_BLOCK: u32 = 2;

struct RecoveryImage {
    image: Mutex<File>,
    overlay: Mutex<BTreeMap<usize, Vec<u8>>>,
    crash_snapshot: Mutex<Option<BTreeMap<usize, Vec<u8>>>>,
    flushes: AtomicUsize,
    snapshot_at_flush: AtomicUsize,
    descriptor_open: AtomicBool,
    descriptor_flushed: AtomicBool,
    commit_had_preflush: AtomicBool,
}

impl RecoveryImage {
    fn open() -> Arc<Self> {
        Arc::new(Self::from_parts(
            File::open(ext4_fixture_path()).expect("open ext4 fixture image"),
            BTreeMap::new(),
        ))
    }

    fn from_parts(image: File, overlay: BTreeMap<usize, Vec<u8>>) -> Self {
        Self {
            image: Mutex::new(image),
            overlay: Mutex::new(overlay),
            crash_snapshot: Mutex::new(None),
            flushes: AtomicUsize::new(0),
            snapshot_at_flush: AtomicUsize::new(usize::MAX),
            descriptor_open: AtomicBool::new(false),
            descriptor_flushed: AtomicBool::new(false),
            commit_had_preflush: AtomicBool::new(false),
        }
    }

    fn snapshot_after_flushes(&self, count: usize) {
        assert!(count > 0);
        *self.crash_snapshot.lock().unwrap() = None;
        self.snapshot_at_flush.store(
            self.flushes.load(Ordering::Relaxed) + count,
            Ordering::Relaxed,
        );
    }

    fn take_crash_snapshot(&self) -> BTreeMap<usize, Vec<u8>> {
        self.snapshot_at_flush.store(usize::MAX, Ordering::Relaxed);
        self.crash_snapshot
            .lock()
            .unwrap()
            .take()
            .expect("armed crash point was not reached")
    }

    fn restore_crash_snapshot(&self) {
        *self.overlay.lock().unwrap() = self.take_crash_snapshot();
    }

    fn crash_clone(&self) -> Arc<Self> {
        let image = self.image.lock().unwrap().try_clone().unwrap();
        Arc::new(Self::from_parts(image, self.take_crash_snapshot()))
    }

    fn reset_journal_order(&self) {
        self.descriptor_open.store(false, Ordering::Relaxed);
        self.descriptor_flushed.store(false, Ordering::Relaxed);
        self.commit_had_preflush.store(false, Ordering::Relaxed);
    }
}

impl BlockDevice for RecoveryImage {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }
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
        if u32::from_be_bytes(buf[..4].try_into().unwrap()) == JBD2_MAGIC {
            match u32::from_be_bytes(buf[4..8].try_into().unwrap()) {
                JBD2_DESCRIPTOR_BLOCK => {
                    self.descriptor_open.store(true, Ordering::Relaxed);
                    self.descriptor_flushed.store(false, Ordering::Relaxed);
                }
                JBD2_COMMIT_BLOCK if self.descriptor_open.load(Ordering::Relaxed) => {
                    self.commit_had_preflush.store(
                        self.descriptor_flushed.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    self.descriptor_open.store(false, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        self.overlay.lock().unwrap().insert(block_id, buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&self) -> Result<(), BlockError> {
        let flush = self.flushes.fetch_add(1, Ordering::Relaxed) + 1;
        if self.descriptor_open.load(Ordering::Relaxed) {
            self.descriptor_flushed.store(true, Ordering::Relaxed);
        }
        if flush == self.snapshot_at_flush.load(Ordering::Relaxed) {
            *self.crash_snapshot.lock().unwrap() = Some(self.overlay.lock().unwrap().clone());
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

fn mounted() -> (Arc<RecoveryImage>, Arc<Ext4FileSystem>) {
    let image = RecoveryImage::open();
    let fs = Ext4FileSystem::new(image.clone()).expect("mount repository ext image");
    (image, fs)
}

#[test]
fn journal_flushes_descriptor_and_data_before_commit_record() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    image.reset_journal_order();
    let root = fs.root_inode().unwrap();
    root.create(
        b"journal-precommit-barrier",
        InodeType::File,
        CreateMetadata {
            mode: 0o644,
            uid: 0,
            gid: 0,
        },
    )
    .unwrap();
    root.sync_storage().unwrap();
    assert!(
        image.commit_had_preflush.load(Ordering::Relaxed),
        "journal commit became writable before descriptor/data durability barrier"
    );
}

#[test]
fn recovery_reloads_allocation_metadata_owners_after_replay() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    image.snapshot_after_flushes(2);
    let file = root
        .create(
            b"replay-allocation-owner",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    // 崩溃点：commit record 已 durable、home checkpoint 尚未完成。
    root.sync_storage().unwrap();
    drop(file);
    drop(root);
    drop(fs);
    image.restore_crash_snapshot();

    let recovered = Ext4FileSystem::new(image).expect("mount committed journal crash snapshot");
    recovered
        .root_inode()
        .unwrap()
        .find_child(b"replay-allocation-owner")
        .expect("replayed namespace entry");
}

#[test]
fn recovery_publishes_replayed_orphan_head_before_reclaim() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let before = test_mount_allocation_state(&fs);
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"replay-orphan-owner",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    file.write_storage(0, &[0x5a]).unwrap();
    root.sync_storage().unwrap();

    // 崩溃点：unlink 事务的 commit record 已 durable、home checkpoint 尚未完成。
    image.snapshot_after_flushes(2);
    root.unlink(b"replay-orphan-owner", false).unwrap();
    root.sync_storage().unwrap();
    let recovered =
        Ext4FileSystem::new(image.crash_clone()).expect("mount replayed orphan transaction");

    assert!(matches!(
        recovered
            .root_inode()
            .unwrap()
            .find_child(b"replay-orphan-owner"),
        Err(FileSystemError::NotFound)
    ));
    assert_eq!(
        test_mount_allocation_state(&recovered),
        before,
        "mount must publish the replayed orphan head and reclaim its inode and data"
    );
}

#[test]
fn orphan_reclaim_rereads_successor_under_mutation_owner() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let before = test_mount_allocation_state(&fs);
    let root = fs.root_inode().unwrap();
    let first = root
        .create(
            b"orphan-first",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    let second = root
        .create(
            b"orphan-second",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    let second_number = second.metadata().unwrap().inode as u32;
    root.unlink(b"orphan-first", false).unwrap();
    root.unlink(b"orphan-second", false).unwrap();
    arm_test_orphan_drop(second_number);
    std::thread::scope(|scope| {
        let second_drop = scope.spawn(move || drop(second));
        wait_test_orphan_drop_admission();
        drop(first);
        release_test_orphan_drop();
        second_drop.join().unwrap();
    });
    assert_eq!(
        test_mount_allocation_state(&fs),
        before,
        "second reclaim must use the successor rewritten by the first reclaim"
    );
}

#[test]
fn orphan_drop_defers_while_mutation_owner_is_busy() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let before = test_mount_allocation_state(&fs);
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"deferred-orphan",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    file.write_storage(0, &[0x5a]).unwrap();
    let inode = file.metadata().unwrap().inode as u32;
    root.unlink(b"deferred-orphan", false).unwrap();
    arm_test_orphan_drop(inode);
    std::thread::scope(|scope| {
        let inode_drop = scope.spawn(move || drop(file));
        wait_test_orphan_drop_admission();
        with_test_mutation_lock(&fs, || {
            release_test_orphan_drop();
            inode_drop.join().unwrap();
        });
    });

    root.set_times(Some(1), Some(1)).unwrap();
    assert_eq!(
        test_mount_allocation_state(&fs),
        before,
        "the next task mutation must reclaim an orphan whose Drop could not wait"
    );
}

#[test]
fn reclaimed_inode_final_drop_does_not_reclaim_again() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_image, fs) = mounted();
    let before = test_mount_allocation_state(&fs);
    let root = fs.root_inode().unwrap();
    let file = root
        .create(
            b"closed-unlink",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    file.write_storage(0, &[0x5a]).unwrap();
    let inode = file.metadata().unwrap().inode as u32;
    drop(file);
    arm_test_orphan_drop(inode);
    release_test_orphan_drop();
    // unlink 内已完成回收；其临时 inode Arc 的 final Drop 看到 dtime 后不得再次进入 orphan 回收。
    root.unlink(b"closed-unlink", false).unwrap();
    assert!(
        !test_orphan_drop_admitted(),
        "already reclaimed inode re-entered orphan reclaim"
    );
    assert_eq!(test_mount_allocation_state(&fs), before);
}

#[test]
fn torn_uncommitted_transaction_is_discarded_instead_of_failing_mount() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    // 第一个 flush 是 commit record 之前的 descriptor/data barrier。
    image.snapshot_after_flushes(1);
    root.create(
        b"torn-transaction",
        InodeType::File,
        CreateMetadata {
            mode: 0o644,
            uid: 0,
            gid: 0,
        },
    )
    .unwrap();
    root.sync_storage().unwrap();
    let crashed = image.crash_clone();
    {
        // 模拟 descriptor 已落盘而首个 data slot 仍是旧内容的 torn write。
        let mut overlay = crashed.overlay.lock().unwrap();
        let descriptor = overlay
            .iter()
            .find(|(_, bytes)| {
                u32::from_be_bytes(bytes[..4].try_into().unwrap()) == JBD2_MAGIC
                    && u32::from_be_bytes(bytes[4..8].try_into().unwrap()) == JBD2_DESCRIPTOR_BLOCK
            })
            .map(|(block, _)| *block)
            .expect("descriptor reached the crash snapshot");
        overlay.insert(descriptor + 1, vec![0; BLOCK_SIZE]);
    }
    let recovered =
        Ext4FileSystem::new(crashed).expect("uncommitted torn transaction must be discarded");
    assert!(matches!(
        recovered
            .root_inode()
            .unwrap()
            .find_child(b"torn-transaction"),
        Err(FileSystemError::NotFound)
    ));
}

const MATRIX_FILE: CreateMetadata = CreateMetadata {
    mode: 0o644,
    uid: 0,
    gid: 0,
};
const MATRIX_DIRECTORY: CreateMetadata = CreateMetadata {
    mode: 0o755,
    uid: 0,
    gid: 0,
};
const BLOCK: usize = BLOCK_SIZE;

/// 把 fixture 与崩溃后挂载产生的 overlay 物化为独立镜像，供 e2fsck 裁决。
fn materialize(image: &RecoveryImage, name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("{}-{name}", std::process::id()));
    std::fs::copy(ext4_fixture_path(), &path).expect("copy ext4 fixture");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open materialized image");
    for (block, bytes) in image.overlay.lock().unwrap().iter() {
        use std::io::Write;
        file.seek(SeekFrom::Start((*block * BLOCK_SIZE) as u64))
            .unwrap();
        file.write_all(bytes).unwrap();
    }
    path
}

fn read_exact(inode: &Arc<dyn crate::fs::Inode>, offset: u64, length: usize) -> Vec<u8> {
    let mut bytes = alloc::vec![0xEE; length];
    assert_eq!(inode.read_storage(offset, &mut bytes).unwrap(), length);
    bytes
}

/// 一个 running transaction 内混合 create/mkdir/write/truncate/rename/unlink，在 sync 之前与提交
/// 过程中的每个 barrier 之后崩溃：恢复后必须是完整旧状态或完整新状态，且 e2fsck 零错误。
#[test]
fn crash_at_every_commit_barrier_recovers_old_or_new_state() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    // 提交固定为三个 barrier：descriptor/data → commit record → home checkpoint。
    for crash_after_flush in 0..=3 {
        let (image, fs) = mounted();
        let root = fs.root_inode().unwrap();
        // 已持久的基线状态。
        let keep = root.create(b"keep", InodeType::File, MATRIX_FILE).unwrap();
        keep.write_storage(0, &[0x4b; 8 * BLOCK]).unwrap();
        root.create(b"victim", InodeType::File, MATRIX_FILE)
            .unwrap()
            .write_storage(0, &[0x56; BLOCK])
            .unwrap();
        root.sync_storage().unwrap();

        // 同一 running transaction 中的全部 mutation。
        let directory = root
            .create(b"dir", InodeType::Directory, MATRIX_DIRECTORY)
            .unwrap();
        let directory_inode = directory.metadata().unwrap().inode;
        directory
            .create(b"new", InodeType::File, MATRIX_FILE)
            .unwrap()
            .write_storage(0, &[0x11; 2 * BLOCK])
            .unwrap();
        keep.truncate_storage(3 * BLOCK as u64).unwrap();
        root.rename(b"keep", directory_inode, b"kept", false)
            .unwrap();
        root.unlink(b"victim", false).unwrap();

        let crashed = if crash_after_flush == 0 {
            image.snapshot_after_flushes(1);
            image.flush().unwrap();
            image.crash_clone()
        } else {
            image.snapshot_after_flushes(crash_after_flush);
            root.sync_storage().unwrap();
            image.crash_clone()
        };
        drop((keep, directory, root));
        drop(fs);

        let recovered = Ext4FileSystem::new(crashed.clone())
            .unwrap_or_else(|error| panic!("crash point {crash_after_flush}: mount {error:?}"));
        let root = recovered.root_inode().unwrap();
        let committed = crash_after_flush >= 2;
        if committed {
            let directory = root.find_child(b"dir").expect("committed directory");
            let new = directory.find_child(b"new").expect("committed file");
            assert_eq!(read_exact(&new, 0, 2 * BLOCK), [0x11; 2 * BLOCK]);
            let kept = directory.find_child(b"kept").expect("renamed file");
            assert_eq!(kept.size(), 3 * BLOCK as u64);
            assert_eq!(read_exact(&kept, 0, 3 * BLOCK), [0x4b; 3 * BLOCK]);
            assert!(matches!(
                root.find_child(b"keep"),
                Err(FileSystemError::NotFound)
            ));
            assert!(matches!(
                root.find_child(b"victim"),
                Err(FileSystemError::NotFound)
            ));
        } else {
            assert!(matches!(
                root.find_child(b"dir"),
                Err(FileSystemError::NotFound)
            ));
            let keep = root
                .find_child(b"keep")
                .expect("uncommitted rename discarded");
            assert_eq!(keep.size(), 8 * BLOCK as u64);
            assert_eq!(read_exact(&keep, 0, 8 * BLOCK), [0x4b; 8 * BLOCK]);
            assert!(root.find_child(b"victim").is_ok());
        }
        // mount 期间的 orphan/replay 结果同样必须持久化后再交给 e2fsck。
        root.sync_storage().unwrap();
        drop(root);
        drop(recovered);
        let path = materialize(&crashed, &format!("crash-matrix-{crash_after_flush}.img"));
        crate::ext4_conformance_tests::e2fsck_clean(&path);
        std::fs::remove_file(path).unwrap();
    }
}
