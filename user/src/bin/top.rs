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

// 清屏函数 - 优化版，减少闪烁
fn clear_screen() {
    // 移动到屏幕左上角，但不清屏
    print!("\x1B[H");
}

// 初始化屏幕 - 只在第一次使用
fn init_screen() {
    // 清空整个屏幕并移动到左上角
    print!("\x1B[2J\x1B[H");
    // 隐藏光标以减少闪烁
    print!("\x1B[?25l");
}

// 恢复屏幕状态
fn restore_screen() {
    // 显示光标
    print!("\x1B[?25h");
    // 移动到屏幕底部
    print!("\x1B[999;1H");
}

// 显示系统统计信息
fn display_system_stats() {
    // 获取并显示当前时间
    let current_time = get_current_time();
    let formatted_time = format_timestamp(current_time.tv_sec);
    println!("LiteOS System Monitor - Current Time: {}", formatted_time);
    println!("");

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
        println!(
            "Total: {}  Running: {}  Sleeping: {}  Zombie: {}",
            stats.total_processes,
            stats.running_processes,
            stats.sleeping_processes,
            stats.zombie_processes
        );
        println!(
            "Memory: {}MB total, {}MB used, {}MB free",
            stats.total_memory / (1024 * 1024),
            stats.used_memory / (1024 * 1024),
            stats.free_memory / (1024 * 1024)
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
            // 显示CPU使用率信息
            let cpu_percent = stats.cpu_usage_percent as f32 / 100.0;
            let cpu_display = if cpu_percent.is_finite() { 
                format!("{:.1}", cpu_percent) 
            } else { 
                "N/A".to_string() 
            };
            let user_display = if user_percent.is_finite() { 
                format!("{:.1}", user_percent) 
            } else { 
                "N/A".to_string() 
            };
            let system_display = if system_percent.is_finite() { 
                format!("{:.1}", system_percent) 
            } else { 
                "N/A".to_string() 
            };
            println!(
                "CPU: {}% total, {}s uptime, {}% user, {}% system",
                cpu_display,
                stats.system_uptime / 1_000_000,
                user_display,
                system_display
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

// 格式化时间戳为可读的日期时间字符串
fn format_timestamp(unix_timestamp: u64) -> String {
    // Unix时间戳是从1970-01-01 00:00:00 UTC开始的秒数

    // 计算天数、小时、分钟、秒
    let total_seconds = unix_timestamp;
    let mut days_since_epoch = total_seconds / 86400; // 86400 = 24 * 60 * 60
    let seconds_today = total_seconds % 86400;

    let hours = seconds_today / 3600;
    let minutes = (seconds_today % 3600) / 60;
    let seconds = seconds_today % 60;

    // 更准确的年月日计算
    let mut year = 1970u64;

    // 计算年份
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days_since_epoch >= days_in_year {
            days_since_epoch -= days_in_year;
            year += 1;
        } else {
            break;
        }
    }

    // 计算月份和日期
    let days_in_months = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u64;
    let mut day = days_since_epoch + 1;

    for &days_in_month in &days_in_months {
        if day > days_in_month {
            day -= days_in_month;
            month += 1;
        } else {
            break;
        }
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hours, minutes, seconds
    )
}

// 判断是否为闰年
fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

// 显示进程信息
fn display_process(info: &ProcessInfo) {
    let heap_size = if info.heap_top >= info.heap_base {
        info.heap_top - info.heap_base
    } else {
        0
    };

    // 格式化CPU使用率
    let cpu_percent_val = info.cpu_percent as f32 / 100.0;
    let cpu_percent_str = if cpu_percent_val.is_finite() && cpu_percent_val > 0.0 {
        format!("{:.1}", cpu_percent_val)
    } else if cpu_percent_val.is_finite() {
        "0.0".to_string()
    } else {
        "NaN".to_string()
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


// 交互模式主循环（自动刷新，支持键盘控制）
fn interactive_mode() {
    let mut sort_by = SortBy::Pid; // 默认按PID排序
    let mut reverse = false;
    let mut auto_refresh = true;
    let mut refresh_interval = 1000; // 1秒刷新间隔
    let mut first_run = true;

    loop {
        // 第一次运行时初始化屏幕，之后只移动光标
        if first_run {
            init_screen();
            first_run = false;
        } else {
            clear_screen();
            // 清除从光标位置到屏幕末尾的所有内容
            print!("\x1B[0J");
        }

        display_system_stats();

        // 显示当前设置信息
        let sort_name = match sort_by {
            SortBy::Pid => "PID",
            SortBy::Cpu => "CPU%",
            SortBy::Memory => "Memory",
            SortBy::VRuntime => "VRuntime",
            SortBy::Status => "Status",
        };

        println!(
            "Settings: Sort by {} {}, Auto-refresh: {}, Interval: {}ms",
            sort_name,
            if reverse { "(desc)" } else { "(asc)" },
            if auto_refresh { "ON" } else { "OFF" },
            refresh_interval
        );
        println!("");

        // 显示进程信息
        match display_all_processes_sorted(sort_by, reverse) {
            Ok(()) => {}
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
                if let Some(key) = check_keyboard_input(true) {
                    if handle_key_input(
                        key,
                        &mut sort_by,
                        &mut reverse,
                        &mut auto_refresh,
                        &mut refresh_interval,
                    ) {
                        restore_screen();
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
                if let Some(key) = check_keyboard_input(true) {
                    if handle_key_input(
                        key,
                        &mut sort_by,
                        &mut reverse,
                        &mut auto_refresh,
                        &mut refresh_interval,
                    ) {
                        return;
                    }
                }
            }
        } else {
            // 如果没有自动刷新，则等待按键
            if let Some(key) = check_keyboard_input(false) {
                if handle_key_input(
                    key,
                    &mut sort_by,
                    &mut reverse,
                    &mut auto_refresh,
                    &mut refresh_interval,
                ) {
                    restore_screen();
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
    refresh_interval: &mut u64,
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
            true // 退出程序
        }
        _ => false, // 忽略其他按键
    }
}

// 显示帮助信息
fn show_help() {
    clear_screen();
    print!("\x1B[0J"); // 清除从光标到屏幕末尾
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
