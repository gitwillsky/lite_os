#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use user_lib::*;

// 排序选项枚举
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortBy {
    Pid,      // 按PID排序
    Cpu,      // 按CPU使用率排序
    Memory,   // 按内存使用排序
    VRuntime, // 按虚拟运行时间排序
    Status,   // 按状态排序
}

// 使用新的高精度睡眠实现
fn sleep(ms: usize) {
    sleep_ms(ms as u64);
}

// 清屏函数
fn clear_screen() {
    // 发送ANSI清屏序列
    print!("\x1B[2J\x1B[H");
    // 也尝试用多个换行来清屏（备用方案）
    for _ in 0..50 {
        println!("");
    }
}

fn display_header() {
    println!("LiteOS Top - Advanced Process Monitor v3.0");
    println!("==========================================");

    // 显示当前时间（简化版）
    println!("System Monitor - Real-time Process Information");

    // 显示当前用户信息
    let current_uid = getuid();
    let current_gid = getgid();
    println!("Running as: UID={}, GID={}", current_uid, current_gid);

    println!("");
}

// 显示系统统计信息
fn display_system_stats() {
    println!("System Overview:");

    let mut stats = SystemStats {
        total_processes: 0,
        running_processes: 0,
        sleeping_processes: 0,
        zombie_processes: 0,
        total_memory: 0,
        used_memory: 0,
        free_memory: 0,
        system_uptime: 0,
        cpu_user_time: 0,
        cpu_system_time: 0,
        cpu_idle_time: 0,
        cpu_usage_percent: 0,
    };

    if get_system_stats(&mut stats) == 0 {
        println!("  Total processes: {}", stats.total_processes);
        println!(
            "  Running: {}  Sleeping: {}  Zombie: {}",
            stats.running_processes, stats.sleeping_processes, stats.zombie_processes
        );
        println!(
            "  Memory: {}MB total, {}MB used, {}MB free",
            stats.total_memory / (1024 * 1024),
            stats.used_memory / (1024 * 1024),
            stats.free_memory / (1024 * 1024)
        );

        // 显示CPU使用率信息
        let cpu_percent = stats.cpu_usage_percent as f32 / 100.0;
        println!(
            "  CPU: {:.1}% total, {}s uptime",
            cpu_percent,
            stats.system_uptime / 1_000_000
        );

        if stats.cpu_user_time + stats.cpu_system_time > 0 {
            let user_percent = (stats.cpu_user_time * 10000
                / (stats.cpu_user_time + stats.cpu_system_time))
                as f32
                / 100.0;
            let system_percent = (stats.cpu_system_time * 10000
                / (stats.cpu_user_time + stats.cpu_system_time))
                as f32
                / 100.0;
            println!(
                "  CPU breakdown: {:.1}% user, {:.1}% system",
                user_percent, system_percent
            );
        }
    } else {
        println!("  Failed to get system statistics");
    }

    println!("");
}

// 显示进程表头
fn display_process_header() {
    println!("  PID  PPID   UID   GID  EUID  EGID  STAT PRI NICE   %CPU    VRUN    HEAP   COMMAND");
    println!("-----  ----  ----  ----  ----  ----  ---- --- ----  -----  ------  ------  --------");
}

// 格式化状态显示
fn format_status(status: u32) -> &'static str {
    match status {
        0 => "READY",
        1 => "RUN  ",
        2 => "ZOMB ",
        3 => "SLEEP",
        _ => "UNK  ",
    }
}

// 格式化大小显示
fn format_size(size: usize) -> String {
    if size == 0 {
        String::from("0B")
    } else if size < 1024 {
        format!("{}B", size)
    } else if size < 1024 * 1024 {
        format!("{}K", size / 1024)
    } else {
        format!("{}M", size / (1024 * 1024))
    }
}

// 从字节数组提取进程名
fn extract_process_name(name_bytes: &[u8; 32]) -> String {
    let end_pos = name_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_bytes.len());
    String::from_utf8_lossy(&name_bytes[..end_pos]).into_owned()
}

// 显示进程信息
fn display_process(info: &ProcessInfo) {
    let heap_size = if info.heap_top >= info.heap_base {
        info.heap_top - info.heap_base
    } else {
        0
    };

    // 格式化CPU使用率
    let cpu_percent_str = if info.cpu_percent > 0 {
        format!("{:.1}", info.cpu_percent as f32 / 100.0)
    } else {
        "0.0".to_string()
    };

    let process_name = extract_process_name(&info.name);

    println!(
        "{:5}  {:4}  {:4}  {:4}  {:4}  {:4}  {} {:3}  {:3}  {:5}  {:6}  {:6}  {}",
        info.pid,
        info.ppid,
        info.uid,
        info.gid,
        info.euid,
        info.egid,
        format_status(info.status),
        info.priority,
        info.nice,
        cpu_percent_str,
        if info.vruntime < 1000000 {
            format!("{}us", info.vruntime)
        } else {
            format!("{}ms", info.vruntime / 1000)
        },
        format_size(heap_size),
        if process_name.is_empty() {
            "N/A".to_string()
        } else {
            process_name
        }
    );
}

// 进程比较函数
fn compare_processes(
    a: &ProcessInfo,
    b: &ProcessInfo,
    sort_by: SortBy,
    reverse: bool,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;

    let result = match sort_by {
        SortBy::Pid => a.pid.cmp(&b.pid),
        SortBy::Cpu => b.cpu_percent.cmp(&a.cpu_percent), // CPU使用率默认降序
        SortBy::Memory => {
            let a_heap = if a.heap_top >= a.heap_base {
                a.heap_top - a.heap_base
            } else {
                0
            };
            let b_heap = if b.heap_top >= b.heap_base {
                b.heap_top - b.heap_base
            } else {
                0
            };
            b_heap.cmp(&a_heap) // 内存使用默认降序
        }
        SortBy::VRuntime => b.vruntime.cmp(&a.vruntime), // 虚拟运行时间默认降序
        SortBy::Status => a.status.cmp(&b.status),
    };

    if reverse { result.reverse() } else { result }
}

// 排序进程列表
fn sort_processes(processes: &mut Vec<ProcessInfo>, sort_by: SortBy, reverse: bool) {
    processes.sort_by(|a, b| compare_processes(a, b, sort_by, reverse));
}

// 获取并显示所有进程信息（带排序功能）
fn display_all_processes_sorted(sort_by: SortBy, reverse: bool) -> Result<(), &'static str> {
    // 首先获取进程数量
    let process_count = get_process_count();
    if process_count <= 0 {
        return Err("No processes found or failed to get process count");
    }

    println!("Found {} processes", process_count);
    println!("");

    // 创建缓冲区来存储PIDs
    let mut pids = Vec::with_capacity(process_count as usize);
    for _ in 0..process_count as usize {
        pids.push(0u32);
    }

    // 获取所有进程的PID
    let actual_count = get_process_list(&mut pids);
    if actual_count <= 0 {
        return Err("Failed to get process list");
    }

    // 收集所有进程信息
    let mut processes = Vec::new();
    for i in 0..actual_count as usize {
        let mut info = ProcessInfo {
            pid: 0,
            ppid: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            status: 0,
            priority: 0,
            nice: 0,
            vruntime: 0,
            heap_base: 0,
            heap_top: 0,
            last_runtime: 0,
            total_cpu_time: 0,
            cpu_percent: 0,
            name: [0u8; 32],
        };

        if get_process_info(pids[i], &mut info) == 0 {
            processes.push(info);
        } else {
            // 创建错误条目用于显示
            let error_info = ProcessInfo {
                pid: pids[i],
                ppid: 0,
                uid: 0,
                gid: 0,
                euid: 0,
                egid: 0,
                status: 999, // 错误状态
                priority: 0,
                nice: 0,
                vruntime: 0,
                heap_base: 0,
                heap_top: 0,
                last_runtime: 0,
                total_cpu_time: 0,
                cpu_percent: 0,
                name: [0u8; 32],
            };
            processes.push(error_info);
        }
    }

    // 排序进程
    sort_processes(&mut processes, sort_by, reverse);

    // 显示进程表头
    display_process_header();

    // 显示排序后的进程信息
    for info in &processes {
        if info.status == 999 {
            println!(
                "{:5}  ----  ----  ----  ----  ----  ---- --- ----  -----  ------  ------  ERROR",
                info.pid
            );
        } else {
            display_process(info);
        }
    }

    Ok(())
}

// 向后兼容的显示所有进程信息函数（默认按CPU排序）
fn display_all_processes() -> Result<(), &'static str> {
    display_all_processes_sorted(SortBy::Cpu, false)
}

// 交互模式主循环（自动刷新，支持键盘控制）
fn interactive_mode() {
    let mut sort_by = SortBy::Pid; // 默认按PID排序
    let mut reverse = false;
    let mut should_refresh = true;

    println!("LiteOS Top - Interactive Mode (Auto-refresh enabled)");
    println!(
        "Commands: [a]=Toggle auto-refresh, [c]=CPU%, [m]=Memory, [p]=PID, [v]=VRuntime, [s]=Status, [r]=Reverse, [q]=Quit"
    );
    println!("Note: Due to blocking read(), keyboard input may delay auto-refresh.");
    println!("Press any key to start or wait for auto-refresh...");
    println!("");

    loop {
        // 清屏
        clear_screen();

        // 显示头部信息
        display_header();

        // 显示系统统计
        display_system_stats();

        // 显示排序和刷新信息
        let sort_name = match sort_by {
            SortBy::Pid => "PID",
            SortBy::Cpu => "CPU%",
            SortBy::Memory => "Memory",
            SortBy::VRuntime => "VRuntime",
            SortBy::Status => "Status",
        };
        println!(
            "Sorted by: {} {}",
            sort_name,
            if reverse {
                "(descending)"
            } else {
                "(ascending)"
            }
        );
        println!("");

        // 显示所有进程信息（带排序）
        match display_all_processes_sorted(sort_by, reverse) {
            Ok(()) => {
                println!("");
                println!(
                    "Commands: [a]=Auto-refresh, [c]=CPU%, [m]=Memory, [p]=PID, [v]=VRuntime, [s]=Status, [r]=Reverse, [q]=Quit"
                );
            }
            Err(e) => {
                println!("Error: {}", e);
                println!("Falling back to basic display...");
                display_basic_info();
            }
        }

        sleep(2000);
    }
}

// 基本信息显示（回退方案）
fn display_basic_info() {
    println!("");
    println!("Basic System Information (Fallback Mode):");
    println!("==========================================");

    let current_pid = getpid();
    let current_uid = getuid();
    let current_gid = getgid();
    let current_euid = geteuid();
    let current_egid = getegid();

    // 获取堆信息
    let heap_start = brk(0);
    let heap_current = sbrk(0);
    let heap_size = if heap_current >= heap_start {
        heap_current - heap_start
    } else {
        0
    };

    println!("  Current Process Information:");
    println!("  PID: {}", current_pid);
    println!(
        "  UID: {}, GID: {}, EUID: {}, EGID: {}",
        current_uid, current_gid, current_euid, current_egid
    );
    println!("  Heap: {} bytes", heap_size);
    println!("");
    println!("Note: Enhanced process monitoring requires kernel support.");
    println!("New system calls may not be available yet.");
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    interactive_mode();
    0
}
