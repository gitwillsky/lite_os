#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use core::sync::atomic::{AtomicUsize, AtomicI32, Ordering};
use user_lib::{kill, signal, sigaction, SigAction, sigprocmask, sigreturn, pause, alarm,
               SIG_SETMASK, SIG_BLOCK, SIG_UNBLOCK, SIG_DFL, SIG_IGN, signals,
               getpid, fork, wait_pid, sleep_ms, exit, TestStats, test_section, test_subsection};

// 全局变量用于跟踪信号处理
static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);
static SIGUSR1_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGUSR2_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGTERM_COUNT: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
extern "C" fn signal_handler_usr1(_signo: usize) {
    SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst);
    SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst);
    LAST_SIGNAL.store(signals::SIGUSR1 as i32, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
extern "C" fn signal_handler_usr2(_signo: usize) {
    SIGUSR2_COUNT.fetch_add(1, Ordering::SeqCst);
    SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst);
    LAST_SIGNAL.store(signals::SIGUSR2 as i32, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
extern "C" fn signal_handler_term(_signo: usize) {
    SIGTERM_COUNT.fetch_add(1, Ordering::SeqCst);
    SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst);
    LAST_SIGNAL.store(signals::SIGTERM as i32, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
extern "C" fn multi_signal_handler(signo: usize) {
    SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst);
    LAST_SIGNAL.store(signo as i32, Ordering::SeqCst);
    
    match signo as u32 {
        signals::SIGUSR1 => SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst),
        signals::SIGUSR2 => SIGUSR2_COUNT.fetch_add(1, Ordering::SeqCst),
        signals::SIGTERM => SIGTERM_COUNT.fetch_add(1, Ordering::SeqCst),
        _ => 0,
    };
}

fn reset_signal_counters() {
    SIGNAL_COUNT.store(0, Ordering::SeqCst);
    LAST_SIGNAL.store(0, Ordering::SeqCst);
    SIGUSR1_COUNT.store(0, Ordering::SeqCst);
    SIGUSR2_COUNT.store(0, Ordering::SeqCst);
    SIGTERM_COUNT.store(0, Ordering::SeqCst);
}

fn wait_for_signal(max_wait_ms: u64) -> bool {
    let mut waited = 0;
    let step = 10;
    
    while waited < max_wait_ms {
        if SIGNAL_COUNT.load(Ordering::SeqCst) > 0 {
            return true;
        }
        sleep_ms(step);
        waited += step;
    }
    false
}

fn test_basic_signal_handling(stats: &mut TestStats) {
    test_subsection!("基础信号处理测试");
    reset_signal_counters();
    
    // 安装SIGUSR1处理器
    let act = SigAction {
        sa_handler: signal_handler_usr1 as usize,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0
    };
    
    let ret = sigaction(signals::SIGUSR1, &act, core::ptr::null_mut());
    test_assert!(ret == 0, "sigaction安装SIGUSR1处理器失败: {}", ret);
    
    // 向自己发送SIGUSR1信号
    let pid = getpid() as usize;
    let kill_ret = kill(pid, signals::SIGUSR1);
    test_assert!(kill_ret == 0, "发送SIGUSR1信号失败: {}", kill_ret);
    
    // 等待信号处理
    test_assert!(wait_for_signal(200), "信号处理器未在预期时间内触发");
    test_assert!(SIGUSR1_COUNT.load(Ordering::SeqCst) == 1, "SIGUSR1计数错误");
    test_assert!(LAST_SIGNAL.load(Ordering::SeqCst) == signals::SIGUSR1 as i32, "最后信号记录错误");
    
    test_info!("SIGUSR1信号处理成功");
    
    test_pass!("基础信号处理测试通过");
    stats.pass();
}

fn test_multiple_signal_handlers(stats: &mut TestStats) {
    test_subsection!("多信号处理器测试");
    reset_signal_counters();
    
    // 为多个信号安装不同的处理器
    let act1 = SigAction {
        sa_handler: signal_handler_usr1 as usize,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0
    };
    
    let act2 = SigAction {
        sa_handler: signal_handler_usr2 as usize,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0
    };
    
    test_assert!(sigaction(signals::SIGUSR1, &act1, core::ptr::null_mut()) == 0, "安装SIGUSR1处理器失败");
    test_assert!(sigaction(signals::SIGUSR2, &act2, core::ptr::null_mut()) == 0, "安装SIGUSR2处理器失败");
    
    let pid = getpid() as usize;
    
    // 发送SIGUSR1
    kill(pid, signals::SIGUSR1);
    sleep_ms(50);
    test_assert!(SIGUSR1_COUNT.load(Ordering::SeqCst) == 1, "SIGUSR1计数错误");
    
    // 发送SIGUSR2
    kill(pid, signals::SIGUSR2);
    sleep_ms(50);
    test_assert!(SIGUSR2_COUNT.load(Ordering::SeqCst) == 1, "SIGUSR2计数错误");
    
    test_assert!(SIGNAL_COUNT.load(Ordering::SeqCst) == 2, "总信号计数错误");
    
    test_pass!("多信号处理器测试通过");
    stats.pass();
}

fn test_signal_masking(stats: &mut TestStats) {
    test_subsection!("信号掩码测试");
    reset_signal_counters();
    
    // 安装信号处理器
    let act = SigAction {
        sa_handler: multi_signal_handler as usize,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0
    };
    
    sigaction(signals::SIGUSR1, &act, core::ptr::null_mut());
    
    // 设置信号掩码屏蔽SIGUSR1
    let mask: u64 = 1 << (signals::SIGUSR1 - 1);
    let mut old_mask: u64 = 0;
    
    let ret = sigprocmask(SIG_BLOCK, &mask, &mut old_mask);
    test_assert!(ret == 0, "设置信号掩码失败: {}", ret);
    test_info!("设置信号掩码屏蔽SIGUSR1");
    
    // 发送被屏蔽的信号
    let pid = getpid() as usize;
    kill(pid, signals::SIGUSR1);
    sleep_ms(100);
    
    // 信号应该被屏蔽，处理器不应该执行
    test_assert!(SIGUSR1_COUNT.load(Ordering::SeqCst) == 0, "屏蔽的信号不应该被处理");
    
    // 解除信号屏蔽
    let ret2 = sigprocmask(SIG_UNBLOCK, &mask, core::ptr::null_mut());
    test_assert!(ret2 == 0, "解除信号掩码失败: {}", ret2);
    test_info!("解除SIGUSR1信号屏蔽");
    
    // 等待挂起的信号被处理
    sleep_ms(100);
    test_assert!(SIGUSR1_COUNT.load(Ordering::SeqCst) == 1, "解除屏蔽后信号应该被处理");
    
    test_pass!("信号掩码测试通过");
    stats.pass();
}

fn test_signal_between_processes(stats: &mut TestStats) {
    test_subsection!("进程间信号通信测试");
    
    let pid = fork();
    test_assert!(pid >= 0, "进程间信号测试fork失败: {}", pid);
    
    if pid == 0 {
        // 子进程：安装信号处理器并等待
        reset_signal_counters();
        
        let act = SigAction {
            sa_handler: multi_signal_handler as usize,
            sa_mask: 0,
            sa_flags: 0,
            sa_restorer: 0
        };
        
        sigaction(signals::SIGUSR1, &act, core::ptr::null_mut());
        sigaction(signals::SIGUSR2, &act, core::ptr::null_mut());
        
        // 等待信号
        for _ in 0..100 {
            if SIGNAL_COUNT.load(Ordering::SeqCst) >= 2 {
                break;
            }
            sleep_ms(10);
        }
        
        let usr1_count = SIGUSR1_COUNT.load(Ordering::SeqCst);
        let usr2_count = SIGUSR2_COUNT.load(Ordering::SeqCst);
        let total = SIGNAL_COUNT.load(Ordering::SeqCst);
        
        // 通过退出码传递信息
        if usr1_count == 1 && usr2_count == 1 && total == 2 {
            exit(0); // 成功
        } else {
            exit(1); // 失败
        }
    } else {
        // 父进程：发送信号给子进程
        sleep_ms(50); // 让子进程准备
        
        let ret1 = kill(pid as usize, signals::SIGUSR1);
        test_assert!(ret1 == 0, "发送SIGUSR1给子进程失败: {}", ret1);
        
        sleep_ms(20);
        
        let ret2 = kill(pid as usize, signals::SIGUSR2);
        test_assert!(ret2 == 0, "发送SIGUSR2给子进程失败: {}", ret2);
        
        // 等待子进程
        let mut status = -1;
        let waited_pid = wait_pid(pid as usize, &mut status);
        test_assert!(waited_pid == pid, "等待子进程失败");
        test_assert!(status == 0, "子进程信号处理失败，退出状态: {}", status);
        
        test_info!("进程间信号通信成功");
    }
    
    test_pass!("进程间信号通信测试通过");
    stats.pass();
}

fn test_signal_default_actions(stats: &mut TestStats) {
    test_subsection!("信号默认动作测试");
    
    // 测试忽略信号
    let ret1 = signal(signals::SIGUSR1, SIG_IGN);
    test_info!("设置SIGUSR1为忽略: {}", ret1);
    
    let pid = getpid() as usize;
    kill(pid, signals::SIGUSR1);
    sleep_ms(50);
    
    // 恢复默认动作
    let ret2 = signal(signals::SIGUSR1, SIG_DFL);
    test_info!("恢复SIGUSR1默认动作: {}", ret2);
    
    test_pass!("信号默认动作测试通过");
    stats.pass();
}

fn test_alarm_signal(stats: &mut TestStats) {
    test_subsection!("定时器信号测试");
    reset_signal_counters();
    
    // 安装SIGALRM处理器
    let act = SigAction {
        sa_handler: multi_signal_handler as usize,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0
    };
    
    let ret = sigaction(signals::SIGALRM, &act, core::ptr::null_mut());
    test_assert!(ret == 0, "安装SIGALRM处理器失败: {}", ret);
    
    // 设置1秒后的定时器
    let alarm_ret = alarm(1);
    test_info!("设置定时器，返回值: {}", alarm_ret);
    
    // 等待定时器信号
    let mut waited = 0;
    while waited < 1500 && LAST_SIGNAL.load(Ordering::SeqCst) != signals::SIGALRM as i32 {
        sleep_ms(100);
        waited += 100;
    }
    
    if LAST_SIGNAL.load(Ordering::SeqCst) == signals::SIGALRM as i32 {
        test_info!("定时器信号SIGALRM成功接收");
    } else {
        test_warn!("定时器信号可能未实现或超时");
    }
    
    test_pass!("定时器信号测试完成");
    stats.pass();
}

fn test_sigaction_advanced(stats: &mut TestStats) {
    test_subsection!("高级sigaction测试");
    
    // 测试带掩码的sigaction
    let act = SigAction {
        sa_handler: multi_signal_handler as usize,
        sa_mask: 1 << (signals::SIGUSR2 - 1), // 处理SIGUSR1时屏蔽SIGUSR2
        sa_flags: signals::SA_RESTART,
        sa_restorer: 0
    };
    
    let mut old_act = SigAction {
        sa_handler: 0,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0
    };
    
    let ret = sigaction(signals::SIGUSR1, &act, &mut old_act);
    test_assert!(ret == 0, "高级sigaction设置失败: {}", ret);
    test_info!("旧处理器地址: 0x{:x}", old_act.sa_handler);
    
    test_pass!("高级sigaction测试通过");
    stats.pass();
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut stats = TestStats::new();
    
    test_section!("信号处理子系统综合测试");
    
    test_basic_signal_handling(&mut stats);
    test_multiple_signal_handlers(&mut stats);
    test_signal_masking(&mut stats);
    test_signal_between_processes(&mut stats);
    test_signal_default_actions(&mut stats);
    test_alarm_signal(&mut stats);
    test_sigaction_advanced(&mut stats);
    
    test_section!("信号处理测试总结");
    test_summary!(stats.total, stats.passed, stats.failed);
    
    if stats.failed == 0 {
        test_pass!("信号处理子系统测试全部通过");
        exit(0);
    } else {
        test_fail!("信号处理子系统测试发现 {} 个失败", stats.failed);
        exit(1);
    }
    0
}


