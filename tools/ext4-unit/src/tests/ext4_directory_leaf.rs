//! 目录 leaf 校验和失败路径：结果仍是 `InvalidFileSystem`，现场诊断只读、不 panic、不污染 cache。

use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
    process::Command,
};

use super::{DIRECTORY, FILE, FS_BLOCK, mount, scratch_copy};
use crate::{
    InodeType,
    ext4_cost_tests::COST_TEST_LOCK,
    fs::{FileSystem, FileSystemError},
};

/// 用 debugfs 取目录第 0 个逻辑块的物理块号。
fn leaf_block(image: &PathBuf, name: &str) -> u64 {
    let output = Command::new(std::env::var_os("LITEOS_DEBUGFS").expect("LITEOS_DEBUGFS"))
        .args(["-R", &format!("bmap /{name} 0")])
        .arg(image)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn corrupt_directory_leaf_is_rejected_and_the_diagnostic_does_not_change_the_result() {
    let _case = COST_TEST_LOCK.lock().unwrap();
    let path = scratch_copy("dir-leaf-corrupt.img");
    {
        let fs = mount(&path);
        let root = fs.root_inode().unwrap();
        let dir = root
            .create(b"leafdir", InodeType::Directory, DIRECTORY)
            .unwrap();
        dir.create(b"entry", InodeType::File, FILE).unwrap();
        dir.sync_storage().unwrap();
        root.sync_storage().unwrap();
    }
    // 未损坏时能正常查找。
    {
        let fs = mount(&path);
        let root = fs.root_inode().unwrap();
        let dir = root.find_child(b"leafdir").unwrap();
        assert!(dir.find_child(b"entry").is_ok());
    }

    // 翻转第一个 record（`.`）名字的一个字节：tail 完好、checksum 失效，与竞态里读到半新半旧块的症状一致。
    let block = leaf_block(&path, "leafdir");
    let mut image = OpenOptions::new().write(true).open(&path).unwrap();
    image
        .seek(SeekFrom::Start(block * FS_BLOCK as u64 + 8))
        .unwrap();
    image.write_all(&[0x7e]).unwrap();
    drop(image);

    // 重新挂载（cache 为空）：查找得到 InvalidFileSystem，诊断路径跑完且不 panic；
    // 失败不会在 metadata cache 里留下坏块：再查一次仍是同一个错误。
    let fs = mount(&path);
    let root = fs.root_inode().unwrap();
    let dir = root.find_child(b"leafdir").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            dir.find_child(b"entry"),
            Err(FileSystemError::InvalidFileSystem)
        ));
    }
}
