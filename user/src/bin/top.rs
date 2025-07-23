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


// 非阻塞检查键盘输入
fn check_keyboard_input() -> Option<u8> {
    // 注意：目前LiteOS的read实现还不完全支持非阻塞模式
    // 这个函数使用简化的实现，在真实系统中应该：
    // 1. 首先设置stdin为非阻塞模式：fcntl(0, F_SETFL, O_NONBLOCK)
    // 2. 然后调用read，如果返回EAGAIN则表示没有数据
    let mut buffer = [0u8; 1];

    // 使用 read 系统调用尝试读取
    // 在完整实现中，这里应该返回EAGAIN错误而不是阻塞
    match read(0, &mut buffer) {
        1 => Some(buffer[0]), // 成功读取到一个字符
        _ => None,            // 没有输入或出错
    }
}

// 交互模式主循环（自动刷新，支持键盘控制）
fn interactive_mode() {
    let mut sort_by = SortBy::Pid; // 默认按PID排序
    let mut reverse = false;
    let mut auto_refresh = true;
    let mut refresh_interval = 1000; // 1秒刷新间隔

    // 显示初始帮助信息
    clear_screen();
    println!("LiteOS Top - Interactive Process Monitor v3.0");
    println!("==============================================");
    println!("");
    println!("Interactive Commands:");
    println!("  [p] - Sort by PID");
    println!("  [c] - Sort by CPU%");
    println!("  [m] - Sort by Memory usage");
    println!("  [v] - Sort by Virtual runtime");
    println!("  [s] - Sort by Status");
    println!("  [r] - Reverse sort order");
    println!("  [a] - Toggle auto-refresh");
    println!("  [f] - Force refresh now");
    println!("  [1] - Set refresh to 1 second");
    println!("  [3] - Set refresh to 3 seconds");
    println!("  [5] - Set refresh to 5 seconds");
    println!("  [h] - Show this help");
    println!("  [q] - Quit");
    println!("");
    println!("Press any key to start monitoring...");

    // 等待用户按键开始
    let mut buffer = [0u8; 1];
    let _ = read(0, &mut buffer);

    loop {
        // 清屏并显示内容
        clear_screen();
        display_header();
        display_system_stats();

        // 显示当前设置信息
        let sort_name = match sort_by {
            SortBy::Pid => "PID",
            SortBy::Cpu => "CPU%",
            SortBy::Memory => "Memory",
            SortBy::VRuntime => "VRuntime",
            SortBy::Status => "Status",
        };

        println!("Settings: Sort by {} {}, Auto-refresh: {}, Interval: {}ms",
            sort_name,
            if reverse { "(desc)" } else { "(asc)" },
            if auto_refresh { "ON" } else { "OFF" },
            refresh_interval
        );
        println!("");

        // 显示进程信息
        match display_all_processes_sorted(sort_by, reverse) {
            Ok(()) => {
                println!("");
                println!("Commands: [p]PID [c]CPU% [m]Memory [v]VRuntime [s]Status [r]Reverse [a]Auto-refresh [q]Quit [h]Help");
            }
            Err(e) => {
                println!("Error: {}", e);
                display_basic_info();
            }
        }

        // 如果启用自动刷新，则等待指定时间并检查按键
        if auto_refresh {
            // 分割等待时间，每100ms检查一次按键
            let check_intervals = refresh_interval / 100;
            let mut key_pressed = false;

            for _ in 0..check_intervals {
                sleep(100);
                if let Some(key) = check_keyboard_input() {
                    if handle_key_input(key, &mut sort_by, &mut reverse, &mut auto_refresh, &mut refresh_interval) {
                        return; // 退出程序
                    }
                    key_pressed = true;
                    break;
                }
            }

            // 如果按了键就不等待剩余时间，立即刷新
            if !key_pressed {
                // 等待剩余时间
                sleep((refresh_interval % 100) as usize);
                // 再次检查按键
                if let Some(key) = check_keyboard_input() {
                    if handle_key_input(key, &mut sort_by, &mut reverse, &mut auto_refresh, &mut refresh_interval) {
                        return;
                    }
                }
            }
        } else {
            // 如果没有自动刷新，则等待按键
            let mut buffer = [0u8; 1];
            if read(0, &mut buffer) == 1 {
                if handle_key_input(buffer[0], &mut sort_by, &mut reverse, &mut auto_refresh, &mut refresh_interval) {
                    return;
                }
            }
        }
    }
}

// 处理键盘输入
fn handle_key_input(
    key: u8,
    sort_by: &mut SortBy,
    reverse: &mut bool,
    auto_refresh: &mut bool,
    refresh_interval: &mut u64
) -> bool {
    match key as char {
        'p' | 'P' => {
            *sort_by = SortBy::Pid;
            false
        }
        'c' | 'C' => {
            *sort_by = SortBy::Cpu;
            false
        }
        'm' | 'M' => {
            *sort_by = SortBy::Memory;
            false
        }
        'v' | 'V' => {
            *sort_by = SortBy::VRuntime;
            false
        }
        's' | 'S' => {
            *sort_by = SortBy::Status;
            false
        }
        'r' | 'R' => {
            *reverse = !*reverse;
            false
        }
        'a' | 'A' => {
            *auto_refresh = !*auto_refresh;
            false
        }
        'f' | 'F' => {
            // 强制刷新，什么都不做，让循环继续
            false
        }
        '1' => {
            *refresh_interval = 1000; // 1秒
            false
        }
        '3' => {
            *refresh_interval = 3000; // 3秒
            false
        }
        '5' => {
            *refresh_interval = 5000; // 5秒
            false
        }
        'h' | 'H' => {
            show_help();
            false
        }
        'q' | 'Q' => {
            println!("Exiting top...");
            true // 退出程序
        }
        _ => false // 忽略其他按键
    }
}

// 显示帮助信息
fn show_help() {
    clear_screen();
    println!("LiteOS Top - Help");
    println!("================");
    println!("");
    println!("Interactive Commands:");
    println!("  [p] - Sort by PID (Process ID)");
    println!("  [c] - Sort by CPU% (CPU usage percentage)");
    println!("  [m] - Sort by Memory usage (heap size)");
    println!("  [v] - Sort by Virtual runtime");
    println!("  [s] - Sort by Status (Ready/Running/Zombie/Sleep)");
    println!("  [r] - Reverse current sort order");
    println!("  [a] - Toggle auto-refresh on/off");
    println!("  [f] - Force refresh display now");
    println!("  [1] - Set refresh interval to 1 second");
    println!("  [3] - Set refresh interval to 3 seconds");
    println!("  [5] - Set refresh interval to 5 seconds");
    println!("  [h] - Show this help screen");
    println!("  [q] - Quit the program");
    println!("");
    println!("Process Status Codes:");
    println!("  READY - Process is ready to run");
    println!("  RUN   - Process is currently running");
    println!("  ZOMB  - Zombie process (finished but not reaped)");
    println!("  SLEEP - Process is sleeping/blocked");
    println!("");
    println!("Press any key to return to process monitor...");

    // 等待按键
    let mut buffer = [0u8; 1];
    let _ = read(0, &mut buffer);
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
