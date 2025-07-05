#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exec, fork, wait, yield_};

#[unsafe(no_mangle)]
fn main() -> i32 {
    // fork 后子进程也会返回 pid （相当于是克隆了父进程）
    // 原进程返回创建的子进程的 Pid，子进程返回 0
    let pid = fork();
    if pid == 0 {
        exec("user_shell\0");
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let pid = wait(&mut exit_code);
            if pid == -1 {
                yield_();
                continue;
            }
            println!(
                "initproc: child process {} exited with code {}",
                pid, exit_code
            );
        }
    }
    0
}
