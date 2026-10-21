//! 以 e2fsprogs `e2fsck -fn` 为裁决者，验证 kernel 写出的 ext4 结构符合固定 profile。
//!
//! 测试在 fixture 的可写副本上执行 htree 转换与分裂、深层 extent tree、截断、跨目录 rename、
//! fast/slow symlink、hard link 与 orphan file crash recovery，最后要求 e2fsck 报告零错误。

use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::Command,
    sync::Mutex,
};

use alloc::{format, sync::Arc, vec::Vec};

use crate::{
    InodeType,
    block::{BLOCK_SIZE, BlockDevice, BlockError},
    ext4_cost_tests::{COST_TEST_LOCK, ext4_fixture_path},
    fs::{
        CreateMetadata, DirectoryEntry, DirectoryVisit, DirectoryVisitor, FileSystem,
        FileSystemError, Inode, ext4::Ext4FileSystem,
    },
};

const FILE: CreateMetadata = CreateMetadata {
    mode: 0o644,
    uid: 0,
    gid: 0,
};
const DIRECTORY: CreateMetadata = CreateMetadata {
    mode: 0o755,
    uid: 0,
    gid: 0,
};
const FS_BLOCK: usize = 4096;

#[path = "tests/ext4_async_read.rs"]
mod async_read;
#[path = "tests/ext4_directory_leaf.rs"]
mod directory_leaf;
#[path = "tests/ext4_xattr.rs"]
mod xattr;

#[test]
fn content_mutation_uses_live_inode_mode_and_current_writer_capabilities() {
    use crate::fs::{AccessIdentity, OwnerModeChange};
    use syscall_abi::capability::Capability;
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("write-capabilities.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let inode = root
        .create(
            b"cap-write",
            InodeType::File,
            CreateMetadata {
                mode: 0o666,
                uid: 2000,
                gid: 1000,
            },
        )
        .unwrap();
    let writer = AccessIdentity::new(1000, 1000, None, crate::namespace::boot(), 0, 1000);
    let outsider = AccessIdentity::new(3000, 3000, None, crate::namespace::boot(), 0, 3000);
    let privileged = AccessIdentity::new(
        3000,
        3000,
        None,
        crate::namespace::boot(),
        Capability::FSETID.mask(),
        3000,
    );
    let mode = |value| {
        inode
            .change_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), value))
            .unwrap()
    };
    let special = || inode.metadata().unwrap().mode & 0o6000;
    mode(0o6777);
    assert_eq!(inode.write_storage(0, b"a", Some(&writer)), Ok(1));
    assert_eq!(special(), 0); // non-owner writer must not need FOWNER
    mode(0o2600);
    inode.append_storage(b"b", Some(&writer)).unwrap();
    assert_eq!(special(), 0o2000); // no IXGRP and writer belongs to inode group
    inode.write_storage(0, b"c", Some(&outsider)).unwrap();
    assert_eq!(special(), 0);
    mode(0o6777);
    inode.write_storage(0, b"d", Some(&privileged)).unwrap();
    assert_eq!(special(), 0o6000);
    inode.write_storage(0, &[], Some(&outsider)).unwrap();
    assert_eq!(special(), 0o6000);
    inode
        .truncate_storage(inode.size(), Some(&outsider))
        .unwrap();
    assert_eq!(special(), 0); // equal-size truncate still removes privilege
    mode(0o6777);
    inode.truncate_storage(0, Some(&privileged)).unwrap();
    assert_eq!(special(), 0o6000);
    inode
        .writeback_storage_batch(&mut |storage| storage.write(0, b"e").map(|_| ()))
        .unwrap();
    assert_eq!(special(), 0o6000); // background owner is never a caller policy
    inode.allocate_storage(8192, 4096, Some(&outsider)).unwrap();
    assert_eq!(special(), 0);
    mode(0o6777);
    inode.prepare_cached_write(&outsider).unwrap();
    assert_eq!(special(), 0); // pre-copy commit survives a later content failure
    let before = inode.metadata().unwrap();
    assert_eq!(
        inode.write_storage(u64::MAX, b"f", Some(&outsider)),
        Err(FileSystemError::NoSpace)
    );
    assert_eq!(inode.metadata().unwrap().mode, before.mode); // journal abort restores the same inode preimage
    root.sync_storage().unwrap();
    drop(inode);
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn first_extent_allocation_no_space_preserves_committed_killpriv() {
    use crate::fs::{AccessIdentity, OwnerModeChange};
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("fallocate-killpriv-full.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let target = root
        .create(b"killpriv-target", InodeType::File, FILE)
        .unwrap();
    let fill = root.create(b"fill-space", InodeType::File, FILE).unwrap();
    let blocks = fs.statistics().unwrap().blocks;
    let mut exhausted = false;
    for index in 0..=blocks {
        match fill.allocate_storage(index * FS_BLOCK as u64, FS_BLOCK as u64, None) {
            Ok(()) => (),
            Err(FileSystemError::NoSpace) => {
                exhausted = true;
                break;
            }
            Err(error) => panic!("unexpected allocation {error:?}"),
        }
    }
    assert!(exhausted);
    target
        .change_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o6777))
        .unwrap();
    let caller = AccessIdentity::new(1000, 1000, None, crate::namespace::boot(), 0, 1000);
    assert_eq!(
        target.allocate_storage(0, FS_BLOCK as u64, Some(&caller)),
        Err(FileSystemError::NoSpace)
    );
    assert_eq!(target.metadata().unwrap().mode & 0o6000, 0);
    assert_eq!(target.size(), 0);
    root.sync_storage().unwrap();
    drop(target);
    drop(fill);
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
    std::fs::remove_file(path).unwrap();
}

/// 直接读写镜像副本的 block device；flush 为 no-op（host 进程退出前文件内容已写入）。
struct WritableImage(Mutex<File>);

struct RemountImage {
    image: WritableImage,
    writes: core::sync::atomic::AtomicUsize,
    flushes: core::sync::atomic::AtomicUsize,
    fail_flush: core::sync::atomic::AtomicBool,
}

impl BlockDevice for RemountImage {
    fn disk_name(&self) -> &[u8] {
        self.image.disk_name()
    }
    fn block_size(&self) -> usize {
        self.image.block_size()
    }
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        self.image.read_block(block_id, buf)
    }
    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        self.writes
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.image.write_block(block_id, buf)
    }
    fn flush(&self) -> Result<(), BlockError> {
        self.flushes
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if self
            .fail_flush
            .swap(false, core::sync::atomic::Ordering::Relaxed)
        {
            Err(BlockError::IoError)
        } else {
            self.image.flush()
        }
    }
    fn block_count(&self) -> u64 {
        self.image.block_count()
    }
}

#[test]
fn ext4_remount_roundtrips_reported_options_and_retries_failed_writable_preparation() {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("remount-options.img");
    let disk = Arc::new(RemountImage {
        image: WritableImage(Mutex::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        )),
        writes: AtomicUsize::new(0),
        flushes: AtomicUsize::new(0),
        fail_flush: AtomicBool::new(false),
    });
    let fs = Ext4FileSystem::new(disk.clone(), crate::fs::DeviceNumber::new(8, 0), true).unwrap();
    let namespace = crate::namespace::boot();
    let writes = disk.writes.load(Ordering::Relaxed);
    let flushes = disk.flushes.load(Ordering::Relaxed);
    for invalid in [
        b"data=writeback".as_slice(),
        b"data=journal",
        b"data",
        b"unknown=1",
    ] {
        assert_eq!(
            fs.remount(invalid, &namespace, true),
            Err(FileSystemError::InvalidOperation)
        );
        assert_eq!(disk.writes.load(Ordering::Relaxed), writes);
        assert_eq!(disk.flushes.load(Ordering::Relaxed), flushes);
    }
    let reported = fs.show_mount_options().unwrap();
    fs.remount(&reported, &namespace, false).unwrap();
    assert_eq!(disk.writes.load(Ordering::Relaxed), writes);
    disk.fail_flush.store(true, Ordering::Relaxed);
    assert_eq!(
        fs.remount(&reported, &namespace, true),
        Err(FileSystemError::IoError)
    );
    assert_eq!(disk.flushes.load(Ordering::Relaxed), flushes + 1);
    fs.remount(&reported, &namespace, true).unwrap();
    let completed = disk.flushes.load(Ordering::Relaxed);
    assert!(
        completed > flushes + 1,
        "failed preparation must perform I/O again"
    );
    fs.remount(&reported, &namespace, true).unwrap();
    assert_eq!(disk.flushes.load(Ordering::Relaxed), completed);
    drop(fs);
    e2fsck_clean(&path);
}

impl BlockDevice for WritableImage {
    fn disk_name(&self) -> &[u8] {
        b"vda"
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        let mut file = self.0.lock().unwrap();
        file.seek(SeekFrom::Start((block_id * BLOCK_SIZE) as u64))
            .map_err(|_| BlockError::IoError)?;
        file.read_exact(buf).map_err(|_| BlockError::IoError)?;
        Ok(buf.len())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        let mut file = self.0.lock().unwrap();
        file.seek(SeekFrom::Start((block_id * BLOCK_SIZE) as u64))
            .map_err(|_| BlockError::IoError)?;
        file.write_all(buf).map_err(|_| BlockError::IoError)?;
        Ok(buf.len())
    }

    fn flush(&self) -> Result<(), BlockError> {
        Ok(())
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.0.lock().unwrap().metadata().unwrap().len() / BLOCK_SIZE as u64
    }
}

struct Names(Vec<Vec<u8>>);

impl DirectoryVisitor for Names {
    fn visit(
        &mut self,
        _next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError> {
        self.0.push(entry.name.to_vec());
        Ok(DirectoryVisit::Continue)
    }
}

/// 每次只取 `limit` 项后停止，验证 hash cursor 跨调用恰好覆盖每项一次。
struct Limited {
    names: Vec<Vec<u8>>,
    limit: usize,
    cursor: u64,
}

impl DirectoryVisitor for Limited {
    fn visit(
        &mut self,
        next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError> {
        if self.limit == 0 {
            return Ok(DirectoryVisit::Stop);
        }
        self.limit -= 1;
        self.names.push(entry.name.to_vec());
        self.cursor = next_cursor;
        Ok(DirectoryVisit::Continue)
    }
}

fn scratch_copy(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{}-{name}", std::process::id()));
    std::fs::copy(ext4_fixture_path(), &path).expect("copy ext4 fixture");
    path
}

/// 以任意块设备挂载（异步读测试用手动完成的设备）。
fn mount_with(device: Arc<dyn BlockDevice>) -> Arc<Ext4FileSystem> {
    Ext4FileSystem::new(device, crate::fs::DeviceNumber::new(8, 0), false)
        .expect("mount ext4 image")
}

fn mount(path: &PathBuf) -> Arc<Ext4FileSystem> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open writable ext4 image");
    Ext4FileSystem::new(
        Arc::new(WritableImage(Mutex::new(file))),
        crate::fs::DeviceNumber::new(8, 0),
        false,
    )
    .expect("mount ext4 image")
}

pub(crate) fn e2fsck_clean(path: &PathBuf) {
    let e2fsck =
        std::env::var_os("LITEOS_E2FSCK").expect("LITEOS_E2FSCK is unset; run `make verify-unit`");
    let output = Command::new(e2fsck)
        .arg("-fn")
        .arg(path)
        .output()
        .expect("run e2fsck");
    assert!(
        output.status.success(),
        "e2fsck rejected kernel-written ext4:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn child(parent: &Arc<dyn Inode>, name: &[u8]) -> Arc<dyn Inode> {
    parent.find_child(name).expect("child exists")
}

fn read_all(directory: &Arc<dyn Inode>) -> Vec<Vec<u8>> {
    let mut names = Names(Vec::new());
    directory.read_directory(0, &mut names).unwrap();
    names.0
}

#[test]
fn kernel_written_htree_extents_and_orphans_pass_e2fsck() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("ext4-conformance.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();

    // 1. 数千个 entry：单 block 线性目录转 htree，随后多次 leaf 分裂。
    let directory = root
        .create(b"conf", InodeType::Directory, DIRECTORY)
        .unwrap();
    const ENTRIES: usize = 3000;
    for index in 0..ENTRIES {
        directory
            .create(
                format!("entry-{index:05}").as_bytes(),
                InodeType::File,
                FILE,
            )
            .unwrap();
    }
    for index in (0..ENTRIES).step_by(3) {
        directory
            .unlink(format!("entry-{index:05}").as_bytes(), false)
            .unwrap();
    }
    let expected: BTreeSet<Vec<u8>> = (0..ENTRIES)
        .filter(|index| index % 3 != 0)
        .map(|index| format!("entry-{index:05}").into_bytes())
        .chain([b".".to_vec(), b"..".to_vec()])
        .collect();
    let listed = read_all(&directory);
    assert_eq!(
        listed.len(),
        expected.len(),
        "readdir must return each entry once"
    );
    assert_eq!(listed.into_iter().collect::<BTreeSet<_>>(), expected);
    // 分批 readdir：每批 97 项后停止，cursor 恢复后既不重复也不遗漏。
    let mut resumed = Vec::new();
    let mut cursor = 0;
    loop {
        let mut batch = Limited {
            names: Vec::new(),
            limit: 97,
            cursor,
        };
        let read = directory.read_directory(cursor, &mut batch).unwrap();
        resumed.extend(batch.names);
        if read.eof {
            break;
        }
        cursor = read.cursor;
    }
    assert_eq!(resumed.len(), expected.len());
    assert_eq!(resumed.into_iter().collect::<BTreeSet<_>>(), expected);
    for index in [1, 1499, 2999] {
        assert!(
            directory
                .find_child(format!("entry-{index:05}").as_bytes())
                .is_ok()
        );
    }
    assert!(matches!(
        directory.find_child(b"entry-00003"),
        Err(FileSystemError::NotFound)
    ));

    // 2. 跨目录 rename：普通文件与 directory（改写 `..`）。
    let nested = directory
        .create(b"nested", InodeType::Directory, DIRECTORY)
        .unwrap();
    let nested_inode = nested.metadata().unwrap().inode;
    for index in (1..300).filter(|index| index % 3 != 0) {
        let name = format!("entry-{index:05}");
        directory
            .rename(name.as_bytes(), nested_inode, name.as_bytes(), false)
            .unwrap();
    }
    let root_inode = root.metadata().unwrap().inode;
    directory
        .rename(b"nested", root_inode, b"moved", false)
        .unwrap();
    let moved = child(&root, b"moved");
    assert_eq!(
        child(&moved, b"..").metadata().unwrap().inode,
        root_inode,
        "moved directory must point `..` at its new parent"
    );

    // 3. 交错写入制造数千个不连续 extent，迫使 extent tree 增高，再分段截断。
    let big = root.create(b"big", InodeType::File, FILE).unwrap();
    const EXTENTS: u64 = 2000;
    for index in 0..EXTENTS {
        let byte = (index % 251) as u8;
        big.write_storage(index * 2 * FS_BLOCK as u64, &[byte; FS_BLOCK], None)
            .unwrap();
    }
    let mut check = [0u8; FS_BLOCK];
    for index in [0, 777, EXTENTS - 1] {
        check.fill(0xAA);
        let read = big
            .read_storage(index * 2 * FS_BLOCK as u64, &mut check)
            .unwrap();
        assert_eq!(read, FS_BLOCK);
        assert!(check.iter().all(|byte| *byte == (index % 251) as u8));
    }
    for index in [0, 777, EXTENTS - 2] {
        check.fill(0xAA);
        let read = big
            .read_storage((index * 2 + 1) * FS_BLOCK as u64, &mut check)
            .unwrap();
        assert_eq!(read, FS_BLOCK);
        assert!(
            check.iter().all(|byte| *byte == 0),
            "hole must read as zero"
        );
    }
    big.truncate_storage(EXTENTS * FS_BLOCK as u64 + 123, None)
        .unwrap();
    big.truncate_storage(37 * FS_BLOCK as u64, None).unwrap();

    // 4. fast/slow symlink 与 hard link。
    root.symlink(b"short-link", b"big", FILE).unwrap();
    root.symlink(b"long-link", &[b'x'; 200], FILE).unwrap();
    assert_eq!(child(&root, b"long-link").read_link().unwrap(), [b'x'; 200]);
    root.link(b"big-alias", child(&root, b"big")).unwrap();

    // 5. 打开期间 unlink 进入 orphan file；故意泄漏 inode 模拟 crash，remount 时回收。
    let open = root
        .create(b"open-unlinked", InodeType::File, FILE)
        .unwrap();
    open.write_storage(0, &[7; 3 * FS_BLOCK], None).unwrap();
    root.unlink(b"open-unlinked", false).unwrap();
    // 写回是延迟的：只有 sync 之后的状态承诺在崩溃后可见。
    root.sync_storage().unwrap();
    core::mem::forget(open);
    drop((big, moved, nested, directory, root));
    core::mem::forget(fs);

    let remounted = mount(&path);
    let root = remounted.root_inode().unwrap();
    assert!(matches!(
        root.find_child(b"open-unlinked"),
        Err(FileSystemError::NotFound)
    ));
    let big = child(&root, b"big");
    assert_eq!(big.size(), 37 * FS_BLOCK as u64);
    check.fill(0xAA);
    assert_eq!(
        big.read_storage(36 * FS_BLOCK as u64, &mut check).unwrap(),
        FS_BLOCK
    );
    assert!(
        check.iter().all(|byte| *byte == 18),
        "block 36 holds extent 18"
    );
    // 没有 unmount：sync 是 e2fsck 裁决前唯一的持久化边界。
    root.sync_storage().unwrap();
    drop((big, root));
    drop(remounted);
    e2fsck_clean(&path);
}

#[test]
fn htree_second_index_level_passes_e2fsck() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("ext4-two-level.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let directory = root
        .create(b"wide", InodeType::Directory, DIRECTORY)
        .unwrap();
    let target = root.create(b"target", InodeType::File, FILE).unwrap();
    // 200-byte 名称让每个 leaf 只容纳十余项；hard link 不消耗 fixture 有限的 inode。
    const LINKS: usize = 12_000;
    let name = |index: usize| format!("{index:06}-{}", "n".repeat(193)).into_bytes();
    for index in 0..LINKS {
        directory.link(&name(index), target.clone()).unwrap();
    }
    for index in (0..LINKS).step_by(5) {
        directory.unlink(&name(index), false).unwrap();
    }
    let listed = read_all(&directory);
    assert_eq!(listed.len(), LINKS - LINKS / 5 + 2);
    assert_eq!(listed.iter().collect::<BTreeSet<_>>().len(), listed.len());
    for index in [1, 6001, LINKS - 1] {
        assert!(directory.find_child(&name(index)).is_ok());
    }
    root.sync_storage().unwrap();
    drop((target, directory, root));
    drop(fs);
    e2fsck_clean(&path);
}

#[test]
fn fifo_and_device_nodes_persist_and_pass_e2fsck() {
    use crate::fs::DeviceNumber;
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("ext4-special.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let node = |mode| CreateMetadata {
        mode,
        uid: 0,
        gid: 0,
    };

    root.mknod(b"pipe", InodeType::Fifo, node(0o600), None)
        .unwrap();
    // major、minor 都小于 256 走旧编码；大 minor 走新编码。
    root.mknod(
        b"null",
        InodeType::CharacterDevice,
        node(0o666),
        Some(DeviceNumber::new(1, 3)),
    )
    .unwrap();
    root.mknod(
        b"disk",
        InodeType::BlockDevice,
        node(0o660),
        Some(DeviceNumber::new(8, 300)),
    )
    .unwrap();
    // kind 与 device 不匹配被拒绝，且不留下目录项。
    assert_eq!(
        root.mknod(
            b"bad",
            InodeType::Fifo,
            node(0o600),
            Some(DeviceNumber::new(1, 1))
        )
        .err(),
        Some(FileSystemError::InvalidOperation)
    );
    assert_eq!(
        root.mknod(b"bad", InodeType::CharacterDevice, node(0o600), None)
            .err(),
        Some(FileSystemError::InvalidOperation)
    );
    assert!(root.find_child(b"bad").is_err());
    root.sync_storage().unwrap();
    drop((root, fs));

    // 重新挂载后类型、设备号与权限从磁盘读回。
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let pipe = child(&root, b"pipe");
    assert_eq!(pipe.inode_type(), InodeType::Fifo);
    assert_eq!(pipe.metadata().unwrap().mode & 0o7777, 0o600);
    assert_eq!(pipe.device_number(), None);
    let null = child(&root, b"null");
    assert_eq!(null.inode_type(), InodeType::CharacterDevice);
    assert_eq!(null.device_number(), Some(DeviceNumber::new(1, 3)));
    let disk = child(&root, b"disk");
    assert_eq!(disk.inode_type(), InodeType::BlockDevice);
    assert_eq!(
        disk.metadata().unwrap().device,
        Some(DeviceNumber::new(8, 300))
    );

    // 删除（含 open-unlinked 的最后引用）后 inode 被回收，e2fsck 不报孤儿或位图错误。
    root.unlink(b"pipe", false).unwrap();
    root.unlink(b"null", false).unwrap();
    drop((pipe, null));
    root.unlink(b"disk", false).unwrap();
    drop(disk);
    root.sync_storage().unwrap();
    drop(root);
    drop(fs);
    e2fsck_clean(&path);
}

#[test]
fn inode_stat_device_is_explicit_disk_identity_not_cache_key() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, _) = crate::ext4_cost_tests::mounted();
    let number = crate::fs::DeviceNumber::new(0x1234, 0x56789);
    let filesystem =
        Ext4FileSystem::new(image, number, true).expect("mount fixture with explicit device");
    let root = filesystem.root_inode().expect("root inode");
    assert_eq!(root.metadata().unwrap().filesystem, number.encode());
    assert_eq!(
        crate::fs::DeviceNumber::decode(root.metadata().unwrap().filesystem),
        number
    );
}

#[test]
fn canonical_ext4_inode_excludes_exec_and_writers_across_hard_links() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("exec-write-exclusion");
    let filesystem = mount(&path);
    let root = filesystem.root_inode().unwrap();
    let inode = root
        .create(
            b"program",
            InodeType::File,
            CreateMetadata {
                mode: 0o100755,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    root.link(b"alias", inode.clone()).unwrap();
    let alias = root.find_child(b"alias").unwrap();
    let writer = crate::fs::InodeAccessLease::write(inode.clone()).unwrap();
    assert!(matches!(
        crate::fs::InodeAccessLease::execute(alias.clone()),
        Err(FileSystemError::TextBusy)
    ));
    drop(writer);
    let execution = crate::fs::InodeAccessLease::execute(inode.clone()).unwrap();
    let same_execution = crate::fs::InodeAccessLease::execute(alias.clone()).unwrap();
    assert!(matches!(
        crate::fs::InodeAccessLease::write(alias.clone()),
        Err(FileSystemError::TextBusy)
    ));
    drop(execution);
    assert!(matches!(
        crate::fs::InodeAccessLease::write(inode.clone()),
        Err(FileSystemError::TextBusy)
    ));
    drop(same_execution);
    drop(crate::fs::InodeAccessLease::write(alias).unwrap());
}

#[test]
fn storage_reads_do_not_mutate_atime_and_path_touch_preserves_ctime() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("path-access-time");
    let filesystem = mount(&path);
    let root = filesystem.root_inode().unwrap();
    let inode = root
        .create(
            b"readable",
            InodeType::File,
            CreateMetadata {
                mode: 0o100644,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap();
    inode.write_storage(0, b"data", None).unwrap();
    inode.set_times(Some(2), Some(3)).unwrap();
    let before = inode.metadata().unwrap();
    let mut data = [0; 4];
    assert_eq!(inode.read_storage(0, &mut data).unwrap(), 4);
    assert_eq!(data, *b"data");
    assert_eq!(inode.metadata().unwrap().atime, before.atime);
    let now = before.mtime.max(before.ctime).saturating_add(10);
    inode.touch_access(now, true).unwrap();
    let after = inode.metadata().unwrap();
    assert_eq!(after.atime, now);
    assert_eq!((after.mtime, after.ctime), (before.mtime, before.ctime));
    inode.touch_access(now + 1, true).unwrap();
    assert_eq!(inode.metadata().unwrap().atime, now);
    inode.touch_access(now + 1, false).unwrap();
    assert_eq!(inode.metadata().unwrap().atime, now + 1);
}
