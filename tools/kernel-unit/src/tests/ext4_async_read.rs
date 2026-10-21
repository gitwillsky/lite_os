//! ext4 的不阻塞读：用“手动完成”的块设备验证步进读在每个等待点让出、完成后继续、取消时回收，
//! 且全程不走同步 `read_block`。内容以真实 e2fsprogs 镜像上内核写出的文件为准。

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use super::{FILE, FS_BLOCK, WritableImage, e2fsck_clean, mount_with, scratch_copy};
use crate::{
    InodeType,
    block::{BlockDevice, BlockError, CompletionNotify, PendingRead},
    ext4_cost_tests::COST_TEST_LOCK,
    fs::{FileSystem, Inode, StorageRead, StorageStep, ext4::clear_test_metadata_cache},
};

/// 块读由测试决定何时完成；同步读被计数，用来证明步进读不走它。
struct ManualImage {
    image: WritableImage,
    sync_reads: AtomicUsize,
    submitted: AtomicUsize,
    dropped: Arc<AtomicUsize>,
    held: StdMutex<Vec<Arc<AtomicBool>>>,
    would_block: AtomicBool,
    /// 提交成功但完成时报告 I/O 错误（设备在途中失败）。
    fail_completion: AtomicBool,
    /// 提交时就报告 I/O 错误。
    fail_submit: AtomicBool,
}

impl ManualImage {
    /// 让当前在途的块读全部完成，返回个数。
    fn release(&self) -> usize {
        let mut held = self.held.lock().unwrap();
        for done in held.iter() {
            done.store(true, Ordering::Release);
        }
        let count = held.len();
        held.clear();
        count
    }
}

struct ManualRead {
    fail: bool,
    bytes: Vec<u8>,
    done: Arc<AtomicBool>,
    dropped: Arc<AtomicUsize>,
}

impl PendingRead for ManualRead {
    fn is_complete(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn finish(self: Box<Self>, output: &mut [u8]) -> Result<(), BlockError> {
        assert!(self.is_complete(), "finish before completion");
        if self.fail {
            return Err(BlockError::IoError);
        }
        output.copy_from_slice(&self.bytes);
        Ok(())
    }
}

impl Drop for ManualRead {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

impl BlockDevice for ManualImage {
    fn disk_name(&self) -> &[u8] {
        b"vda"
    }
    fn block_size(&self) -> usize {
        self.image.block_size()
    }
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        self.sync_reads.fetch_add(1, Ordering::Relaxed);
        self.image.read_block(block_id, buf)
    }
    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        self.image.write_block(block_id, buf)
    }
    fn flush(&self) -> Result<(), BlockError> {
        self.image.flush()
    }
    fn block_count(&self) -> u64 {
        self.image.block_count()
    }
    fn begin_read(
        self: Arc<Self>,
        block_id: usize,
        _notify: Arc<dyn CompletionNotify>,
    ) -> Result<Box<dyn PendingRead>, BlockError> {
        if self.would_block.load(Ordering::Relaxed) {
            return Err(BlockError::WouldBlock);
        }
        if self.fail_submit.load(Ordering::Relaxed) {
            return Err(BlockError::IoError);
        }
        let mut bytes = vec![0; BLOCK_SIZE_BYTES];
        self.image.read_block(block_id, &mut bytes)?;
        let done = Arc::new(AtomicBool::new(false));
        self.held.lock().unwrap().push(done.clone());
        self.submitted.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(ManualRead {
            fail: self.fail_completion.load(Ordering::Relaxed),
            bytes,
            done,
            dropped: self.dropped.clone(),
        }))
    }
}

const BLOCK_SIZE_BYTES: usize = FS_BLOCK;

struct Silent;
impl CompletionNotify for Silent {
    fn notify(&self) {}
}

fn pattern(file: u8, block: usize) -> Vec<u8> {
    (0..FS_BLOCK)
        .map(|i| (i as u8).wrapping_mul(31) ^ file ^ (block as u8).wrapping_mul(7))
        .collect()
}

fn manual(path: &std::path::PathBuf) -> Arc<ManualImage> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    Arc::new(ManualImage {
        image: WritableImage(StdMutex::new(file)),
        sync_reads: AtomicUsize::new(0),
        submitted: AtomicUsize::new(0),
        dropped: Arc::new(AtomicUsize::new(0)),
        held: StdMutex::new(Vec::new()),
        would_block: AtomicBool::new(false),
        fail_completion: AtomicBool::new(false),
        fail_submit: AtomicBool::new(false),
    })
}

/// 驱动到 `Done`，每次 `Wait` 就让在途的块读完成；返回 (读到的字节数, 等待次数)。
fn drive(read: &mut dyn StorageRead, device: &ManualImage, output: &mut [u8]) -> (usize, usize) {
    let mut waits = 0;
    loop {
        match read.step(output).unwrap() {
            StorageStep::Done(bytes) => return (bytes, waits),
            StorageStep::Wait => {
                waits += 1;
                assert!(device.release() > 0, "Wait without an in-flight block read");
            }
            StorageStep::Fallback => panic!("unexpected fallback"),
        }
    }
}

/// 交错写两个文件，使 `A` 的块不连续：超过 4 个 extent，extent tree 因此有一层索引节点。
fn fragmented_pair(fs: &Arc<crate::fs::ext4::Ext4FileSystem>) -> (Arc<dyn Inode>, Arc<dyn Inode>) {
    let root = fs.root_inode().unwrap();
    let a = root.create(b"async-a", InodeType::File, FILE).unwrap();
    let b = root.create(b"async-b", InodeType::File, FILE).unwrap();
    for block in 0..12 {
        a.write_storage((block * FS_BLOCK) as u64, &pattern(1, block), None)
            .unwrap();
        b.write_storage((block * FS_BLOCK) as u64, &pattern(2, block), None)
            .unwrap();
    }
    a.sync_storage().unwrap();
    b.sync_storage().unwrap();
    (a, b)
}

#[test]
fn cold_read_waits_for_extent_node_and_data_without_synchronous_io() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("async-read.img");
    let device = manual(&path);
    let fs = mount_with(device.clone());
    let (a, _b) = fragmented_pair(&fs);
    clear_test_metadata_cache(&fs);
    let sync_before = device.sync_reads.load(Ordering::Relaxed);

    let mut read = a
        .begin_storage_read(0, 12 * FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .expect("ext4 supports non-blocking reads");
    let mut output = vec![0u8; 12 * FS_BLOCK];
    let (bytes, waits) = drive(read.as_mut(), &device, &mut output);

    assert_eq!(bytes, 12 * FS_BLOCK);
    for block in 0..12 {
        assert_eq!(
            &output[block * FS_BLOCK..(block + 1) * FS_BLOCK],
            &pattern(1, block)[..],
            "block {block}"
        );
    }
    // 至少：一次索引节点 + 每个数据块各一次。
    assert!(waits > 12, "waits = {waits}");
    assert_eq!(
        device.sync_reads.load(Ordering::Relaxed),
        sync_before,
        "a non-blocking read must not use the synchronous read path"
    );
    drop(read);
    drop(a);
    drop(_b);
    drop(fs);
    e2fsck_clean(&path);
}

#[test]
fn warm_metadata_cache_needs_no_extra_wait_and_holes_read_as_zero() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("async-read-hole.img");
    let device = manual(&path);
    let fs = mount_with(device.clone());
    let root = fs.root_inode().unwrap();
    let sparse = root.create(b"sparse", InodeType::File, FILE).unwrap();
    sparse
        .write_storage((3 * FS_BLOCK) as u64, &pattern(3, 3), None)
        .unwrap();
    sparse.sync_storage().unwrap();

    let mut read = sparse
        .begin_storage_read(0, 4 * FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    let mut output = vec![0xeeu8; 4 * FS_BLOCK];
    let (bytes, waits) = drive(read.as_mut(), &device, &mut output);
    assert_eq!(bytes, 4 * FS_BLOCK);
    assert!(output[..3 * FS_BLOCK].iter().all(|byte| *byte == 0));
    assert_eq!(&output[3 * FS_BLOCK..], &pattern(3, 3)[..]);
    // 三个 hole 不读设备；只有最后一个数据块需要等待（extent 在 inode 里，没有索引节点）。
    assert_eq!(waits, 1);
    drop(read);
    drop(sparse);
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
}

#[test]
fn eof_bounds_the_read_and_reads_past_eof_are_empty() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("async-read-eof.img");
    let device = manual(&path);
    let fs = mount_with(device.clone());
    let root = fs.root_inode().unwrap();
    let file = root.create(b"short", InodeType::File, FILE).unwrap();
    file.write_storage(0, &pattern(4, 0)[..1000], None).unwrap();
    file.sync_storage().unwrap();

    let mut output = vec![0u8; 4096];
    let mut read = file
        .begin_storage_read(900, 4096, Arc::new(Silent))
        .unwrap()
        .unwrap();
    let (bytes, _) = drive(read.as_mut(), &device, &mut output);
    assert_eq!(bytes, 100);
    assert_eq!(&output[..100], &pattern(4, 0)[900..1000]);
    let mut read = file
        .begin_storage_read(5000, 4096, Arc::new(Silent))
        .unwrap()
        .unwrap();
    assert_eq!(drive(read.as_mut(), &device, &mut output).0, 0);
    drop(read);
    drop(file);
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
}

#[test]
fn dropping_a_read_mid_flight_releases_the_pending_block_read() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("async-read-cancel.img");
    let device = manual(&path);
    let fs = mount_with(device.clone());
    let (a, b) = fragmented_pair(&fs);
    clear_test_metadata_cache(&fs);

    let mut read = a
        .begin_storage_read(0, 12 * FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    let mut output = vec![0u8; 12 * FS_BLOCK];
    assert_eq!(read.step(&mut output).unwrap(), StorageStep::Wait);
    let dropped_before = device.dropped.load(Ordering::Relaxed);
    drop(read);
    assert_eq!(
        device.dropped.load(Ordering::Relaxed),
        dropped_before + 1,
        "the in-flight block read must be dropped with the read"
    );
    // 取消没有毒化缓存：重新读得到完整正确的内容。
    device.release();
    let mut again = a
        .begin_storage_read(0, 12 * FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    assert_eq!(drive(again.as_mut(), &device, &mut output).0, 12 * FS_BLOCK);
    assert_eq!(&output[..FS_BLOCK], &pattern(1, 0)[..]);
    drop(again);
    drop(a);
    drop(b);
    drop(fs);
    e2fsck_clean(&path);
}

#[test]
fn a_full_request_table_falls_back_instead_of_failing() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("async-read-busy.img");
    let device = manual(&path);
    let fs = mount_with(device.clone());
    let root = fs.root_inode().unwrap();
    let file = root.create(b"busy", InodeType::File, FILE).unwrap();
    file.write_storage(0, &pattern(5, 0), None).unwrap();
    file.sync_storage().unwrap();
    device.would_block.store(true, Ordering::Relaxed);

    let mut read = file
        .begin_storage_read(0, FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    let mut output = vec![0u8; FS_BLOCK];
    assert_eq!(read.step(&mut output).unwrap(), StorageStep::Fallback);
    drop(read);
    drop(file);
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
}

/// 设备在提交时或在途中失败：读以错误结束（不 panic、不挂起、不回退到同步路径重试），
/// 取消后请求槽被归还，之后设备恢复时同一个 inode 仍能读到正确内容（失败没有毒化 cache）。
#[test]
fn device_errors_end_the_read_and_do_not_poison_the_cache() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("async-read-fault.img");
    let device = manual(&path);
    let fs = mount_with(device.clone());
    let root = fs.root_inode().unwrap();
    let file = root.create(b"fault", InodeType::File, FILE).unwrap();
    file.write_storage(0, &pattern(6, 0), None).unwrap();
    file.sync_storage().unwrap();
    clear_test_metadata_cache(&fs);
    let mut output = vec![0u8; FS_BLOCK];

    // 提交时失败。
    device.fail_submit.store(true, Ordering::Relaxed);
    let mut read = file
        .begin_storage_read(0, FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    assert!(
        read.step(&mut output).is_err(),
        "submit failure must surface"
    );
    drop(read);
    device.fail_submit.store(false, Ordering::Relaxed);

    // 完成时失败：先 Wait，设备完成后 step 返回错误。
    device.fail_completion.store(true, Ordering::Relaxed);
    let mut read = file
        .begin_storage_read(0, FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    assert_eq!(read.step(&mut output).unwrap(), StorageStep::Wait);
    assert!(device.release() > 0);
    assert!(
        read.step(&mut output).is_err(),
        "completion failure must surface"
    );
    drop(read);
    device.fail_completion.store(false, Ordering::Relaxed);

    let mut again = file
        .begin_storage_read(0, FS_BLOCK, Arc::new(Silent))
        .unwrap()
        .unwrap();
    assert_eq!(drive(again.as_mut(), &device, &mut output).0, FS_BLOCK);
    assert_eq!(&output[..], &pattern(6, 0)[..]);
    drop(again);
    drop(file);
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
}
