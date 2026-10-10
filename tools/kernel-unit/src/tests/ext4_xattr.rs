//! 实际ext4 adapter写盘→remount→e2fsck裁决；不复制xattr磁盘格式或transaction算法。
use super::*;
use crate::fs::ext4::{
    reset_test_stage_capacity, reset_test_write_costs, set_test_stage_capacity, test_write_costs,
};
use crate::fs::{AccessIdentity, OwnerModeChange, XattrChange};
use syscall_abi::{
    capability::Capability,
    file_capability::{EFFECTIVE, FileCapabilityData, NAME, REVISION_2},
};

struct StageCapacity;
impl Drop for StageCapacity {
    fn drop(&mut self) {
        reset_test_stage_capacity();
    }
}

fn debugfs(path: &PathBuf, request: &str) -> String {
    let output = Command::new(std::env::var_os("LITEOS_DEBUGFS").expect("LITEOS_DEBUGFS"))
        .args(["-w", "-R", request])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn change(
    inode: &dyn Inode,
    name: &[u8],
    value: Option<Vec<u8>>,
    flags: u32,
) -> Result<(), FileSystemError> {
    let root = AccessIdentity::root();
    inode.change_xattr(XattrChange::new(
        name,
        value,
        flags,
        &root,
        crate::namespace::boot(),
    ))
}
fn caps() -> Vec<u8> {
    let (bytes, length) = FileCapabilityData {
        magic: REVISION_2 | EFFECTIVE,
        permitted: Capability::NET_BIND_SERVICE.mask(),
        inheritable: 0,
        root_uid: 0,
    }
    .encode()
    .unwrap();
    bytes[..length].to_vec()
}

#[test]
fn body_external_attributes_survive_replacement_remount_and_failed_capacity_admission() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    crate::namespace::boot();
    let path = std::env::temp_dir().join(format!("{}-xattr-storage.img", std::process::id()));
    // 原生工具只能在创建时关闭 ea_inode；禁止修改已格式化卷的 feature bit。
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let payload = ext4_fixture_path().with_file_name("ext4-init-payload");
    let output = Command::new("python3")
        .arg(repository.join("create_fs.py"))
        .args(["create", "--without-ea-inode", "--size", "64", "--file"])
        .arg(&path)
        .arg("--init")
        .arg(payload)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let inode = root.create(b"attributes", InodeType::File, FILE).unwrap();
    change(&*inode, NAME, Some(caps()), 1).unwrap();
    change(&*inode, b"user.large", Some(vec![0x71; 2300]), 1).unwrap();
    change(&*inode, b"trusted.admin", Some(b"hidden".to_vec()), 1).unwrap();
    change(&*inode, b"user.empty", Some(Vec::new()), 1).unwrap();
    assert_eq!(inode.metadata().unwrap().blocks, 8);
    assert_eq!(
        change(&*inode, b"user.large", Some(vec![3]), 3),
        Err(FileSystemError::AlreadyExists)
    );
    assert_eq!(
        change(&*inode, b"user.missing", Some(vec![3]), 3),
        Err(FileSystemError::NotFound)
    );
    let before = inode.metadata().unwrap();
    assert_eq!(
        change(&*inode, b"user.absent", None, 2),
        Err(FileSystemError::NotFound)
    );
    change(&*inode, b"user.large", Some(vec![0x71; 2300]), 0).unwrap();
    assert_eq!(inode.metadata().unwrap().ctime, before.ctime);
    assert_eq!(
        change(&*inode, b"user.overflow", Some(vec![5; 4096]), 0),
        Err(FileSystemError::NoSpace)
    );
    assert_eq!(
        inode.read_xattr(b"user.overflow"),
        Err(FileSystemError::NotFound)
    );
    assert_eq!(inode.read_xattr(b"user.large").unwrap(), vec![0x71; 2300]);
    assert_eq!(inode.metadata().unwrap().blocks, before.blocks);
    assert_eq!(inode.metadata().unwrap().ctime, before.ctime);
    let unprivileged = AccessIdentity::new(1000, 1000, None, crate::namespace::boot(), 0, 1000);
    let names = inode.list_xattrs(&unprivileged).unwrap();
    assert!(
        !names
            .split(|byte| *byte == 0)
            .any(|name| name == b"trusted.admin")
    );
    assert!(
        inode
            .list_xattrs(&AccessIdentity::root())
            .unwrap()
            .split(|byte| *byte == 0)
            .any(|name| name == b"trusted.admin")
    );
    inode.sync_storage().unwrap();
    drop((inode, root, fs));
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let inode = root.find_child(b"attributes").unwrap();
    assert_eq!(inode.read_xattr(NAME).unwrap(), caps());
    assert_eq!(inode.read_xattr(b"user.empty").unwrap(), b"");
    assert_eq!(inode.read_xattr(b"user.large").unwrap(), vec![0x71; 2300]);
    change(&*inode, b"user.large", Some(vec![0x39; 2700]), 2).unwrap();
    assert_eq!(inode.read_xattr(b"user.large").unwrap(), vec![0x39; 2700]);
    change(&*inode, b"user.large", None, 2).unwrap();
    change(&*inode, b"trusted.admin", None, 2).unwrap();
    assert_eq!(inode.metadata().unwrap().blocks, 0); // 剩下cap+empty全部回到body，旧block正确释放。
    inode.sync_storage().unwrap();
    drop((inode, root, fs));
    e2fsck_clean(&path);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn file_cap_killpriv_is_atomic_with_write_truncate_and_noop_chown() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    crate::namespace::boot();
    let path = scratch_copy("xattr-killpriv.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let inode = root.create(b"capability", InodeType::File, FILE).unwrap();
    inode
        .change_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o6777))
        .unwrap();
    change(&*inode, NAME, Some(caps()), 0).unwrap();
    change(&*inode, b"user.keep", Some(vec![0x33; 2100]), 0).unwrap();
    inode
        .write_storage(0, b"written", Some(&AccessIdentity::root()))
        .unwrap();
    assert_eq!(inode.metadata().unwrap().mode & 0o6000, 0o6000); // FSETID豁免mode，不豁免capability。
    assert_eq!(inode.read_xattr(NAME), Err(FileSystemError::NotFound));
    assert_eq!(inode.read_xattr(b"user.keep").unwrap(), vec![0x33; 2100]);
    change(&*inode, NAME, Some(caps()), 0).unwrap();
    inode
        .truncate_storage(inode.size(), Some(&AccessIdentity::root()))
        .unwrap();
    assert_eq!(inode.read_xattr(NAME), Err(FileSystemError::NotFound));
    change(&*inode, NAME, Some(caps()), 0).unwrap();
    inode
        .change_owner_mode(OwnerModeChange::chown(AccessIdentity::root(), None, None))
        .unwrap();
    assert_eq!(inode.read_xattr(NAME), Err(FileSystemError::NotFound));
    change(&*inode, NAME, Some(Vec::new()), 0).unwrap();
    inode
        .write_storage(0, b"again", Some(&AccessIdentity::root()))
        .unwrap();
    assert_eq!(inode.read_xattr(NAME).unwrap(), b"");
    // 后台writeback从不伪装成caller去权限。
    change(&*inode, NAME, Some(caps()), 0).unwrap();
    inode
        .writeback_storage_batch(&mut |writer| writer.write(0, b"cache").map(|_| ()))
        .unwrap();
    assert_eq!(inode.read_xattr(NAME).unwrap(), caps());
    inode.sync_storage().unwrap();
    drop((inode, root, fs));
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    assert_eq!(
        root.find_child(b"capability")
            .unwrap()
            .read_xattr(NAME)
            .unwrap(),
        caps()
    );
    root.sync_storage().unwrap();
    drop((root, fs));
    e2fsck_clean(&path);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn native_ea_inode_import_and_kernel_maximum_value_replacement_and_final_reclaim() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    crate::namespace::boot();
    let path = scratch_copy("xattr-value-inodes.img");
    let payload = path.with_extension("value");
    let native_value: Vec<u8> = (0..4096).map(|index| (index % 251) as u8).collect();
    std::fs::write(&payload, &native_value).unwrap();
    debugfs(
        &path,
        &format!("ea_set -f {} /bin/init user.native", payload.display()),
    );
    // e2fsprogs 1.47.4 debugfs/xattrs.c 的 -f 只读取 blocksize 字节；
    // libext2fs producer 未更新父 inode EA value 收费。用原生 checker 完成输入卷，
    // kernel 尚未挂载；kernel 修改之后仍只用只读 e2fsck_clean 裁决，不允许修复。
    let prepared = Command::new(std::env::var_os("LITEOS_E2FSCK").expect("LITEOS_E2FSCK"))
        .args(["-f", "-y"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        matches!(prepared.status.code(), Some(0 | 1)),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    e2fsck_clean(&path);
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let bin = root.find_child(b"bin").unwrap();
    let init = bin.find_child(b"init").unwrap();
    assert_eq!(init.read_xattr(b"user.native").unwrap(), native_value);
    // 替换原生 producer 创建的 EA inode，不泄漏旧值 owner。
    change(&*init, b"user.native", Some(vec![0x29; 16384]), 2).unwrap();
    change(&*init, b"user.native", None, 0).unwrap();
    let imported: Vec<u8> = (0..65536).map(|index| (index % 251) as u8).collect();
    let inode = root
        .create(b"large-attributes", InodeType::File, FILE)
        .unwrap();
    let before = fs.statistics().unwrap();
    change(&*inode, b"user.first", Some(imported.clone()), 1).unwrap();
    change(&*inode, b"user.second", Some(vec![0x87; 65536]), 1).unwrap();
    change(&*inode, b"user.small", Some(b"same".to_vec()), 0).unwrap();
    inode.sync_storage().unwrap();
    let metadata = inode.metadata().unwrap();
    let allocated = fs.statistics().unwrap();
    assert_eq!(allocated.files_free + 2, before.files_free);
    assert_eq!(allocated.blocks_free + 32, before.blocks_free);
    reset_test_write_costs();
    change(&*inode, b"user.small", Some(b"same".to_vec()), 2).unwrap();
    inode.sync_storage().unwrap();
    let unchanged = test_write_costs();
    assert_eq!(unchanged.transactions, 0);
    assert_eq!(unchanged.journal_writes, 0);
    assert_eq!(unchanged.home_writes, 0);
    assert_eq!(inode.metadata().unwrap().ctime, metadata.ctime);
    assert_eq!(fs.statistics().unwrap().files_free, allocated.files_free);
    assert_eq!(fs.statistics().unwrap().blocks_free, allocated.blocks_free);
    // 名称总量仍由 body/external block 限定；容量不足时小值也转为真实 EA inode。
    for index in 0..12 {
        change(
            &*inode,
            format!("user.more-{index}").as_bytes(),
            Some(vec![index as u8; 700]),
            0,
        )
        .unwrap();
    }
    inode.sync_storage().unwrap();
    drop((inode, init, bin, root, fs));
    e2fsck_clean(&path);
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let inode = root.find_child(b"large-attributes").unwrap();
    assert_eq!(inode.read_xattr(b"user.first").unwrap(), imported);
    assert_eq!(inode.read_xattr(b"user.second").unwrap(), vec![0x87; 65536]);
    let start = std::time::Instant::now();
    for _ in 0..500 {
        let value = inode.read_xattr(b"user.first").unwrap();
        assert_eq!(value.len(), 65536);
        std::hint::black_box(value);
    }
    eprintln!(
        "EXT4_EA_READ_DIAGNOSTIC bytes=65536 iterations=500 elapsed_ns={}",
        start.elapsed().as_nanos()
    );
    for index in 0..12 {
        let name = format!("user.more-{index}");
        assert_eq!(
            inode.read_xattr(name.as_bytes()).unwrap(),
            vec![index as u8; 700]
        );
        change(&*inode, name.as_bytes(), None, 0).unwrap();
    }
    change(&*inode, b"user.first", Some(vec![0x34; 8192]), 2).unwrap();
    assert_eq!(inode.read_xattr(b"user.first").unwrap(), vec![0x34; 8192]);
    change(&*inode, b"user.first", None, 0).unwrap();
    change(&*inode, b"user.second", None, 0).unwrap();
    change(&*inode, b"user.small", None, 0).unwrap();
    assert_eq!(fs.statistics().unwrap().files_free, before.files_free);
    assert_eq!(fs.statistics().unwrap().blocks_free, before.blocks_free);
    change(&*inode, b"user.unlinked", Some(vec![0x96; 65536]), 0).unwrap();
    root.unlink(b"large-attributes", false).unwrap();
    assert_eq!(
        inode.read_xattr(b"user.unlinked").unwrap(),
        vec![0x96; 65536]
    );
    drop(inode);
    root.sync_storage().unwrap();
    assert_eq!(fs.statistics().unwrap().files_free, before.files_free + 1);
    assert_eq!(fs.statistics().unwrap().blocks_free, before.blocks_free);
    drop((root, fs));
    e2fsck_clean(&path);
    std::fs::remove_file(payload).unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn ea_inode_staging_failure_refunds_allocation_and_preserves_old_value() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    crate::namespace::boot();
    let path = scratch_copy("xattr-value-rollback.img");
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let inode = root.create(b"rollback", InodeType::File, FILE).unwrap();
    change(&*inode, b"user.value", Some(vec![0x17; 8192]), 0).unwrap();
    assert_eq!(inode.metadata().unwrap().blocks, 16); // 父 inode 原生 EA value 收费。
    inode.sync_storage().unwrap();
    let before = fs.statistics().unwrap();
    let metadata = inode.metadata().unwrap();
    {
        let _reset = StageCapacity;
        set_test_stage_capacity(8);
        assert_eq!(
            change(&*inode, b"user.value", Some(vec![0x78; 65536]), 2),
            Err(FileSystemError::NoSpace)
        );
    }
    assert_eq!(inode.read_xattr(b"user.value").unwrap(), vec![0x17; 8192]);
    assert_eq!(inode.metadata().unwrap().ctime, metadata.ctime);
    assert_eq!(fs.statistics().unwrap().files_free, before.files_free);
    assert_eq!(fs.statistics().unwrap().blocks_free, before.blocks_free);
    inode.sync_storage().unwrap();
    drop((inode, root, fs));
    e2fsck_clean(&path);
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    assert_eq!(
        root.find_child(b"rollback")
            .unwrap()
            .read_xattr(b"user.value")
            .unwrap(),
        vec![0x17; 8192]
    );
    drop((root, fs));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn shared_external_block_counts_one_value_holder_and_cow_adds_one_physical_holder() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    crate::namespace::boot();
    let path = scratch_copy("xattr-shared-ea-block.img");
    let name = format!("user.{}", "x".repeat(200));
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let left = root.create(b"ea-left", InodeType::File, FILE).unwrap();
    let right = root.create(b"ea-right", InodeType::File, FILE).unwrap();
    change(&*left, name.as_bytes(), Some(vec![0x16; 8192]), 0).unwrap();
    assert_eq!(left.metadata().unwrap().blocks, 24); // 8 sectors external block + 16 sectors EA value charge.
    root.sync_storage().unwrap();
    drop((left, right, root, fs));
    // 用原生 debugfs 指定共享关联，再由 e2fsck 构造正确 h_refcount/checksum。
    // 测试不复制磁盘编码或引用算法；kernel 接收前必须经严格 -fn 确认为干净卷。
    let stat = debugfs(&path, "stat /ea-left");
    let block: u64 = stat
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("File ACL: ")
                .and_then(|number| number.split_whitespace().next()?.parse().ok())
        })
        .expect("native debugfs File ACL");
    assert_ne!(block, 0);
    debugfs(
        &path,
        &format!("set_inode_field /ea-right file_acl {block}"),
    );
    debugfs(&path, "set_inode_field /ea-right blocks 24");
    let output = Command::new(std::env::var_os("LITEOS_E2FSCK").expect("LITEOS_E2FSCK"))
        .args(["-fy"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    e2fsck_clean(&path);
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let left = root.find_child(b"ea-left").unwrap();
    let right = root.find_child(b"ea-right").unwrap();
    let before = fs.statistics().unwrap();
    assert_eq!(right.read_xattr(name.as_bytes()).unwrap(), vec![0x16; 8192]);
    // unchanged EA value 在新物理 COW block 上多一持有，不能乘旧 block h_refcount。
    change(&*left, b"user.note", Some(b"cow".to_vec()), 0).unwrap();
    assert_eq!(fs.statistics().unwrap().files_free, before.files_free);
    assert_eq!(fs.statistics().unwrap().blocks_free + 1, before.blocks_free);
    root.sync_storage().unwrap();
    drop((left, right, root, fs));
    e2fsck_clean(&path);
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let left = root.find_child(b"ea-left").unwrap();
    let right = root.find_child(b"ea-right").unwrap();
    change(&*left, name.as_bytes(), Some(vec![0x57; 16384]), 2).unwrap();
    assert_eq!(right.read_xattr(name.as_bytes()).unwrap(), vec![0x16; 8192]);
    change(&*left, name.as_bytes(), None, 0).unwrap();
    assert_eq!(fs.statistics().unwrap().files_free, before.files_free);
    assert_eq!(fs.statistics().unwrap().blocks_free, before.blocks_free);
    change(&*right, name.as_bytes(), None, 0).unwrap();
    assert_eq!(fs.statistics().unwrap().files_free, before.files_free + 1);
    assert_eq!(fs.statistics().unwrap().blocks_free, before.blocks_free + 3);
    assert_eq!(left.read_xattr(b"user.note").unwrap(), b"cow");
    root.sync_storage().unwrap();
    drop((left, right, root, fs));
    e2fsck_clean(&path);
    std::fs::remove_file(path).unwrap();
}
