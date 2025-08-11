#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{kill, sigaction, SigAction, sigprocmask, SIG_SETMASK, signals, getpid, sleep_ms, exit};

static HANDLED: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
extern "C" fn handler_sigusr1(_signo: usize) {
    HANDLED.store(1, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("signal: 开始信号接口测试");

    // 安装 SIGUSR1 处理器
    let act = SigAction { sa_handler: handler_sigusr1 as usize, sa_mask: 0, sa_flags: 0, sa_restorer: 0 };
    let r = sigaction(signals::SIGUSR1, &act as *const SigAction, core::ptr::null_mut());
    test_assert!(r == 0, "sigaction 失败: {}", r);

    // 发送信号给自己
    let pid = getpid() as usize;
    let s = kill(pid, signals::SIGUSR1);
    test_assert!(s == 0, "kill 失败: {}", s);

    // 轮询等待处理器置位
    for _ in 0..50 { if HANDLED.load(Ordering::SeqCst) == 1 { break; } let _ = sleep_ms(10); }
    test_assert!(HANDLED.load(Ordering::SeqCst) == 1, "信号处理器未触发");

    // 简测掩码设置/恢复
    let mut old: u64 = 0;
    let set: u64 = 1 << (signals::SIGUSR1 as u64 - 1);
    let r2 = sigprocmask(SIG_SETMASK, &set as *const u64, &mut old as *mut u64);
    test_assert!(r2 == 0, "sigprocmask 失败: {}", r2);

    test_info!("signal: 所有用例通过");
    exit(0);
    0
}


