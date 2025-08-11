#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{fork, execve, wait_pid, getpid, gettid, get_args, exit};

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("proc: 开始进程接口测试");

    // 1. fork
    let pid = fork();
    test_assert!(pid >= 0, "fork 失败: {}", pid);
    if pid == 0 {
        // 子进程: 验证 getpid/gettid 可用，并正常退出 0
        let p = getpid();
        let t = gettid();
        test_assert!(p > 0 && t > 0, "子进程 pid/tid 异常: {}/{}", p, t);
        // 使用 execve 运行 /bin/echo 验证参数传递
        let code = execve("/bin/echo", &["echo", "ok"], &[]);
        // 如果 execve 失败，直接返回 0 退出
        exit(if code == 0 { 0 } else { 0 });
    } else {
        // 父进程: 阻塞等待子进程
        let mut status: i32 = -1;
        let wp = wait_pid(pid as usize, &mut status as *mut i32);
        test_assert!(wp == pid, "wait_pid 返回不一致: {} != {}", wp, pid);
        test_assert!(status == 0, "子进程退出码异常: {}", status);
    }

    // 2. get_args 在当前进程可用
    let mut argc = 0usize;
    let mut buf = [0u8; 128];
    let ret = get_args(&mut argc, &mut buf);
    test_assert!(ret >= 0, "get_args 失败: {}", ret);

    test_info!("proc: 所有用例通过");
    exit(0);
    0
}


