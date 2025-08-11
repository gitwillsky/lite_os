#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{get_process_count, get_process_list, get_process_info, get_system_stats, get_cpu_core_info, ProcessInfo, SystemStats, exit};

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("system: 开始系统信息接口测试");

    let count = get_process_count();
    test_assert!(count >= 0, "get_process_count 失败: {}", count);

    // 拉取进程列表
    let mut pids = [0u32; 64];
    let n = get_process_list(&mut pids);
    test_assert!(n >= 0, "get_process_list 失败: {}", n);
    let n = core::cmp::min(n as usize, pids.len());

    // 查询前几个进程信息
    let mut info = ProcessInfo {
        pid: 0, ppid: 0, uid: 0, gid: 0, euid: 0, egid: 0,
        status: 0, priority: 0, nice: 0, vruntime: 0, heap_base: 0, heap_top: 0,
        last_runtime: 0, total_cpu_time: 0, cpu_percent: 0, core_id: 0, name: [0; 32]
    };
    for &pid in &pids[..n] {
        if pid == 0 { continue; }
        let r = get_process_info(pid, &mut info);
        test_assert!(r == 0 || r == -1, "get_process_info 返回异常: {}", r);
    }

    // 系统统计
    let mut stats = SystemStats {
        total_processes: 0, running_processes: 0, sleeping_processes: 0, zombie_processes: 0,
        total_memory: 0, used_memory: 0, free_memory: 0, system_uptime: 0, cpu_user_time: 0,
        cpu_system_time: 0, cpu_idle_time: 0, cpu_usage_percent: 0
    };
    let rs = get_system_stats(&mut stats);
    test_assert!(rs == 0 || rs == -1, "get_system_stats 返回异常: {}", rs);

    // CPU 核心信息
    let core = get_cpu_core_info();
    if let Some(c) = core {
        test_assert!(c.total_cores >= c.active_cores, "核心信息异常");
    }

    test_info!("system: 所有用例通过");
    exit(0);
    0
}


