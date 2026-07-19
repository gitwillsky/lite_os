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

use crate::ext2_cost_tests::COST_TEST_LOCK;
use crate::{
    InodeType,
    drivers::block::{BLOCK_SIZE, BlockDevice, BlockError},
    fs::{
        CreateMetadata, FileSystem, FileSystemError,
        ext2::{
            Ext2FileSystem, arm_test_orphan_drop, release_test_orphan_drop,
            test_mount_allocation_state, wait_test_orphan_drop_admission,
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
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fs.img");
        Arc::new(Self::from_parts(
            File::open(path).expect("open repository ext image"),
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

fn mounted() -> (Arc<RecoveryImage>, Arc<Ext2FileSystem>) {
    let image = RecoveryImage::open();
    let fs = Ext2FileSystem::new(image.clone()).expect("mount repository ext image");
    (image, fs)
}

#[test]
fn journal_flushes_descriptor_and_data_before_commit_record() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    image.reset_journal_order();
    fs.root_inode()
        .unwrap()
        .create(
            b"journal-precommit-barrier",
            InodeType::File,
            CreateMetadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
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
    drop(file);
    drop(root);
    drop(fs);
    image.restore_crash_snapshot();

    let recovered = Ext2FileSystem::new(image).expect("mount committed journal crash snapshot");
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

    image.snapshot_after_flushes(2);
    root.unlink(b"replay-orphan-owner", false).unwrap();
    let recovered =
        Ext2FileSystem::new(image.crash_clone()).expect("mount replayed orphan transaction");

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
