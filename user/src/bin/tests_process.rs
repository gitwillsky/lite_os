#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    TestStats, execve, exit, fork, get_args, getegid, geteuid, getgid, getpid, gettid, getuid,
    setegid, seteuid, setgid, setuid, test_section, test_subsection, wait_pid, wait_pid_nb,
};

fn test_process_creation(stats: &mut TestStats) {
    test_subsection!("进程创建和基础信息测试");

    // 测试getpid/gettid
    let pid = getpid();
    let tid = gettid();
    test_assert!(pid > 0, "getpid 返回无效值: {}", pid);
    test_assert!(tid > 0, "gettid 返回无效值: {}", tid);
    test_info!("当前进程 PID: {}, TID: {}", pid, tid);

    // 测试权限信息获取
    let uid = getuid();
    let gid = getgid();
    let euid = geteuid();
    let egid = getegid();
    test_info!(
        "权限信息 - UID: {}, GID: {}, EUID: {}, EGID: {}",
        uid,
        gid,
        euid,
        egid
    );

    test_pass!("进程基础信息获取测试通过");
    stats.pass();
}

fn test_fork_basic(stats: &mut TestStats) {
    test_subsection!("基础fork测试");

    let fork_pid = fork();
    test_assert!(fork_pid >= 0, "fork 失败: {}", fork_pid);

    if fork_pid == 0 {
        // 子进程
        let child_pid = getpid();
        let child_tid = gettid();
        test_info!("子进程 - PID: {}, TID: {}", child_pid, child_tid);
        test_assert!(child_pid > 0 && child_tid > 0, "子进程ID获取失败");
        exit(42);
    } else {
        // 父进程
        let mut status = -1;
        let waited_pid = wait_pid(fork_pid as usize, &mut status);
        test_assert!(
            waited_pid == fork_pid,
            "wait_pid 返回错误: {} != {}",
            waited_pid,
            fork_pid
        );
        test_assert!(status == 42, "子进程退出码错误: {}", status);
        test_info!("父进程成功等待子进程，退出码: {}", status);
    }

    test_pass!("基础fork测试通过");
    stats.pass();
}

fn test_multiple_forks(stats: &mut TestStats) {
    test_subsection!("多子进程fork测试");

    let num_children = 3;
    let mut child_pids = [0isize; 3];

    // 创建多个子进程
    for i in 0..num_children {
        let pid = fork();
        test_assert!(pid >= 0, "fork 第{}个子进程失败: {}", i, pid);

        if pid == 0 {
            // 子进程 - 返回不同的退出码
            let exit_code = (i + 1) * 10;
            test_info!("子进程 {} 将以退出码 {} 退出", i, exit_code);
            exit(exit_code as i32);
        } else {
            child_pids[i] = pid;
            test_info!("创建子进程 {} PID: {}", i, pid);
        }
    }

    // 父进程等待所有子进程
    for i in 0..num_children {
        let mut status = -1;
        let waited_pid = wait_pid(child_pids[i] as usize, &mut status);
        test_assert!(waited_pid == child_pids[i], "等待子进程 {} 失败", i);

        let expected_status = (i + 1) * 10;
        test_assert!(
            status == expected_status as i32,
            "子进程 {} 退出码错误: {} != {}",
            i,
            status,
            expected_status
        );
        test_info!("子进程 {} 正常退出，状态码: {}", i, status);
    }

    test_pass!("多子进程fork测试通过");
    stats.pass();
}

fn test_exec_basic(stats: &mut TestStats) {
    test_subsection!("基础exec测试");

    let fork_pid = fork();
    test_assert!(fork_pid >= 0, "exec测试fork失败: {}", fork_pid);

    if fork_pid == 0 {
        // 子进程尝试执行echo命令
        test_info!("子进程准备执行 /bin/echo");
        let exec_ret = execve("/bin/echo", &["echo", "exec_test_ok"], &[]);

        // 如果execve成功，这里不应该执行到
        // 如果失败，直接退出
        test_info!("execve 返回: {}", exec_ret);
        exit(if exec_ret == 0 { 0 } else { 1 });
    } else {
        // 父进程等待
        let mut status = -1;
        let waited_pid = wait_pid(fork_pid as usize, &mut status);
        test_assert!(waited_pid == fork_pid, "exec测试等待失败");

        // execve成功则子进程应该正常退出
        test_info!("exec测试子进程退出，状态码: {}", status);
    }

    test_pass!("基础exec测试通过");
    stats.pass();
}

fn test_exec_with_args(stats: &mut TestStats) {
    test_subsection!("带参数exec测试");

    let fork_pid = fork();
    test_assert!(fork_pid >= 0, "带参数exec测试fork失败: {}", fork_pid);

    if fork_pid == 0 {
        // 子进程执行带参数的命令
        let args = ["echo", "hello", "world", "from", "exec"];
        let env = ["TEST_VAR=exec_test"];

        test_info!("子进程执行 execve 带参数");
        let exec_ret = execve("/bin/echo", &args, &env);
        exit(if exec_ret == 0 { 0 } else { 2 });
    } else {
        // 父进程等待
        let mut status = -1;
        let waited_pid = wait_pid(fork_pid as usize, &mut status);
        test_assert!(waited_pid == fork_pid, "带参数exec测试等待失败");
        test_info!("带参数exec测试子进程退出，状态码: {}", status);
    }

    test_pass!("带参数exec测试通过");
    stats.pass();
}

fn test_wait_variants(stats: &mut TestStats) {
    test_subsection!("等待进程变体测试");

    // 测试非阻塞等待
    let fork_pid = fork();
    test_assert!(fork_pid >= 0, "等待测试fork失败: {}", fork_pid);

    if fork_pid == 0 {
        // 子进程先睡眠一小段时间
        let sleep_time = user_lib::TimeSpec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        }; // 100ms
        user_lib::nanosleep(&sleep_time, core::ptr::null_mut());
        exit(123);
    } else {
        // 父进程立即非阻塞等待 - 应该返回-2 (EAGAIN)
        let mut status = -1;
        let nb_result = wait_pid_nb(fork_pid as usize, &mut status);
        test_assert!(nb_result == -2, "非阻塞等待应该返回-2: {}", nb_result);
        test_info!("非阻塞等待正确返回EAGAIN");

        // 然后阻塞等待
        let waited_pid = wait_pid(fork_pid as usize, &mut status);
        test_assert!(waited_pid == fork_pid, "阻塞等待失败");
        test_assert!(status == 123, "等待变体测试退出码错误: {}", status);
        test_info!("阻塞等待成功，状态码: {}", status);
    }

    test_pass!("等待进程变体测试通过");
    stats.pass();
}

fn test_get_args(stats: &mut TestStats) {
    test_subsection!("获取进程参数测试");

    let mut argc = 0usize;
    let mut argv_buf = [0u8; 512];

    let ret = get_args(&mut argc, &mut argv_buf);
    test_assert!(ret >= 0, "get_args 失败: {}", ret);
    test_info!("get_args 成功，参数个数: {}", argc);

    if argc > 0 && ret > 0 {
        // 尝试解析第一个参数（程序名）
        let args_str = core::str::from_utf8(&argv_buf[..ret as usize]).unwrap_or("<解析失败>");
        test_info!("参数字符串: {:?}", args_str);
    }

    test_pass!("获取进程参数测试通过");
    stats.pass();
}

fn test_permission_syscalls(stats: &mut TestStats) {
    test_subsection!("权限系统调用测试");

    let orig_uid = getuid();
    let orig_gid = getgid();
    let orig_euid = geteuid();
    let orig_egid = getegid();

    test_info!(
        "原始权限 - UID:{} GID:{} EUID:{} EGID:{}",
        orig_uid,
        orig_gid,
        orig_euid,
        orig_egid
    );

    // 尝试设置相同的权限（应该成功）
    let ret1 = setuid(orig_uid);
    let ret2 = setgid(orig_gid);
    let ret3 = seteuid(orig_euid);
    let ret4 = setegid(orig_egid);

    test_info!(
        "权限设置结果 - setuid:{} setgid:{} seteuid:{} setegid:{}",
        ret1,
        ret2,
        ret3,
        ret4
    );

    // 验证权限未改变
    test_assert!(getuid() == orig_uid, "UID 意外改变");
    test_assert!(getgid() == orig_gid, "GID 意外改变");
    test_assert!(geteuid() == orig_euid, "EUID 意外改变");
    test_assert!(getegid() == orig_egid, "EGID 意外改变");

    test_pass!("权限系统调用测试通过");
    stats.pass();
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut stats = TestStats::new();

    test_section!("进程管理子系统综合测试");

    test_process_creation(&mut stats);
    test_fork_basic(&mut stats);
    test_multiple_forks(&mut stats);
    test_exec_basic(&mut stats);
    test_exec_with_args(&mut stats);
    test_wait_variants(&mut stats);
    test_get_args(&mut stats);
    test_permission_syscalls(&mut stats);

    test_section!("进程管理测试总结");
    test_summary!(stats.total, stats.passed, stats.failed);

    if stats.failed == 0 {
        test_pass!("进程管理子系统测试全部通过");
        exit(0);
    } else {
        test_fail!("进程管理子系统测试发现 {} 个失败", stats.failed);
        exit(1);
    }
    0
}
