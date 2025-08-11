#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{mkdir, chdir, getcwd, open, write, read, lseek, close, remove, exit};
use user_lib::open_flags;

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("fs: 开始文件系统测试");

    // 1. 创建并进入目录
    let ret = mkdir("/tmp_test");
    test_assert!(ret == 0 || ret == -17, "mkdir 失败: {}", ret); // -EEXIST 也允许
    let r = chdir("/tmp_test");
    test_assert!(r == 0, "chdir 失败: {}", r);
    let mut cwd = [0u8; 256];
    let n = getcwd(&mut cwd);
    test_assert!(n > 0, "getcwd 失败: {}", n);
    let s = core::str::from_utf8(&cwd[..n as usize]).unwrap_or("");
    test_assert!(s.ends_with("/tmp_test") || s == "/tmp_test", "cwd 异常: {}", s);

    // 2. 创建并写入文件
    let fd = open("f.txt", open_flags::O_CREAT | open_flags::O_RDWR | open_flags::O_TRUNC);
    test_assert!(fd >= 0, "open 失败: {}", fd);
    let msg = b"hello world";
    let wn = write(fd as usize, msg);
    test_assert!(wn as usize == msg.len(), "write 长度不匹配: {}", wn);

    // 3. 回到开头读取验证
    let off = lseek(fd as usize, 0, 0);
    test_assert!(off == 0, "lseek 失败: {}", off);
    let mut buf = [0u8; 32];
    let rn = read(fd as usize, &mut buf);
    test_assert!(rn as usize == msg.len(), "read 长度不匹配: {}", rn);
    test_assert!(&buf[..msg.len()] == msg, "读回内容不一致");
    let _ = close(fd as usize);

    // 4. 列目录应包含 f.txt
    let mut lbuf = [0u8; 256];
    let ln = user_lib::listdir(".", &mut lbuf);
    test_assert!(ln >= 0, "listdir 失败: {}", ln);
    let listing = core::str::from_utf8(&lbuf[..ln as usize]).unwrap_or("");
    test_assert!(listing.split('\n').any(|e| e == "f.txt"), "目录不包含 f.txt: {}", listing);

    // 5. 清理：删除文件与目录
    test_assert!(remove("f.txt") == 0, "删除 f.txt 失败");
    let _ = chdir("/");
    let rmdir = remove("/tmp_test");
    test_assert!(rmdir == 0, "删除目录失败: {}", rmdir);

    test_info!("fs: 所有用例通过");
    exit(0);
    0
}


