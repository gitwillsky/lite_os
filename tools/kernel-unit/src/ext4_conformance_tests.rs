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
    drivers::block::{BLOCK_SIZE, BlockDevice, BlockError},
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

/// 直接读写镜像副本的 block device；flush 为 no-op（host 进程退出前文件内容已写入）。
struct WritableImage(Mutex<File>);

impl BlockDevice for WritableImage {
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

fn mount(path: &PathBuf) -> Arc<Ext4FileSystem> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open writable ext4 image");
    Ext4FileSystem::new(Arc::new(WritableImage(Mutex::new(file)))).expect("mount ext4 image")
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
        big.write_storage(index * 2 * FS_BLOCK as u64, &[byte; FS_BLOCK])
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
    big.truncate_storage(EXTENTS * FS_BLOCK as u64 + 123)
        .unwrap();
    big.truncate_storage(37 * FS_BLOCK as u64).unwrap();

    // 4. fast/slow symlink 与 hard link。
    root.symlink(b"short-link", b"big", FILE).unwrap();
    root.symlink(b"long-link", &[b'x'; 200], FILE).unwrap();
    assert_eq!(child(&root, b"long-link").read_link().unwrap(), [b'x'; 200]);
    root.link(b"big-alias", child(&root, b"big")).unwrap();

    // 5. 打开期间 unlink 进入 orphan file；故意泄漏 inode 模拟 crash，remount 时回收。
    let open = root
        .create(b"open-unlinked", InodeType::File, FILE)
        .unwrap();
    open.write_storage(0, &[7; 3 * FS_BLOCK]).unwrap();
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
