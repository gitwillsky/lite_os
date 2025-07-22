#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// 排序选项枚举
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortBy {
    Pid,      // 按PID排序
    Cpu,      // 按CPU使用率排序
    Memory,   // 按内存使用排序
    VRuntime, // 按虚拟运行时间排序
    Status,   // 按状态排序
}

// 简单的睡眠实现
fn sleep(ms: usize) {
    for _ in 0..ms * 1000 {
        yield_();
    }
}

// 获取字符输入（非阻塞）
fn get_char() -> u8 {
    let mut buffer = [0u8; 1];
    if read(0, &mut buffer) > 0 {
        buffer[0]
    } else {
        0
    }
}

// 检查是否有键盘输入
fn has_input() -> bool {
    let mut buffer = [0u8; 1];
    read(0, &mut buffer) > 0
}

// 显示系统头部信息
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
        println!("  Running: {}  Sleeping: {}  Zombie: {}", 
                 stats.running_processes, stats.sleeping_processes, stats.zombie_processes);
        println!("  Memory: {}MB total, {}MB used, {}MB free",
                 stats.total_memory / (1024 * 1024),
                 stats.used_memory / (1024 * 1024),
                 stats.free_memory / (1024 * 1024));
        
        // 显示CPU使用率信息
        let cpu_percent = stats.cpu_usage_percent as f32 / 100.0;
        println!("  CPU: {:.1}% total, {}s uptime",
                 cpu_percent,
                 stats.system_uptime / 1_000_000);
        
        if stats.cpu_user_time + stats.cpu_system_time > 0 {
            let user_percent = (stats.cpu_user_time * 10000 / (stats.cpu_user_time + stats.cpu_system_time)) as f32 / 100.0;
            let system_percent = (stats.cpu_system_time * 10000 / (stats.cpu_user_time + stats.cpu_system_time)) as f32 / 100.0;
            println!("  CPU breakdown: {:.1}% user, {:.1}% system", user_percent, system_percent);
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
    let end_pos = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
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
    
    println!("{:5}  {:4}  {:4}  {:4}  {:4}  {:4}  {} {:3}  {:3}  {:5}  {:6}  {:6}  {}",
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
        if info.vruntime < 1000000 { format!("{}us", info.vruntime) } else { format!("{}ms", info.vruntime / 1000) },
        format_size(heap_size),
        if process_name.is_empty() { "N/A".to_string() } else { process_name }
    );
}

// 进程比较函数
fn compare_processes(a: &ProcessInfo, b: &ProcessInfo, sort_by: SortBy, reverse: bool) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    
    let result = match sort_by {
        SortBy::Pid => a.pid.cmp(&b.pid),
        SortBy::Cpu => b.cpu_percent.cmp(&a.cpu_percent), // CPU使用率默认降序
        SortBy::Memory => {
            let a_heap = if a.heap_top >= a.heap_base { a.heap_top - a.heap_base } else { 0 };
            let b_heap = if b.heap_top >= b.heap_base { b.heap_top - b.heap_base } else { 0 };
            b_heap.cmp(&a_heap) // 内存使用默认降序
        },
        SortBy::VRuntime => b.vruntime.cmp(&a.vruntime), // 虚拟运行时间默认降序
        SortBy::Status => a.status.cmp(&b.status),
    };
    
    if reverse {
        result.reverse()
    } else {
        result
    }
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
            println!("{:5}  ----  ----  ----  ----  ----  ---- --- ----  -----  ------  ------  ERROR",
                     info.pid);
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
    let mut sort_by = SortBy::Cpu; // 默认按CPU排序
    let mut reverse = false;
    let mut should_refresh = true;
    let mut auto_refresh = true; // 默认开启自动刷新
    let mut refresh_counter = 0; // 刷新计数器
    const AUTO_REFRESH_INTERVAL: usize = 20; // 自动刷新间隔（循环次数，约1秒）
    
    println!("LiteOS Top - Interactive Mode (Auto-refresh enabled)");
    println!("Commands:");
    println!("  [Enter/Space] = Manual refresh");
    println!("  [a] = Toggle auto-refresh");
    println!("  [c] = Sort by CPU%");
    println!("  [m] = Sort by Memory");
    println!("  [p] = Sort by PID");
    println!("  [v] = Sort by VRuntime");
    println!("  [s] = Sort by Status");
    println!("  [r] = Reverse sort order");
    println!("  [q] = Quit");
    println!("  [Ctrl+C] = Force quit");
    println!("");
    println!("Press any key to start...");
    
    // 等待用户按键开始
    let _ = get_char();
    
    loop {
        if should_refresh {
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
            println!("Sorted by: {} {}", sort_name, if reverse { "(descending)" } else { "(ascending)" });
            if auto_refresh {
                println!("Auto-refresh: ON (press 'a' to toggle)");
            } else {
                println!("Auto-refresh: OFF (press 'a' to toggle)");
            }
            println!("");
            
            // 显示所有进程信息（带排序）
            match display_all_processes_sorted(sort_by, reverse) {
                Ok(()) => {
                    println!("");
                    println!("Commands: [Enter]=Refresh, [a]=Auto-refresh, [c]=CPU%, [m]=Memory, [p]=PID, [v]=VRuntime, [s]=Status, [r]=Reverse, [q]=Quit");
                },
                Err(e) => {
                    println!("Error: {}", e);
                    println!("Falling back to basic display...");
                    display_basic_info();
                }
            }
            
            should_refresh = false;
        }
        
        // 自动刷新逻辑
        if auto_refresh {
            refresh_counter += 1;
            if refresh_counter >= AUTO_REFRESH_INTERVAL {
                should_refresh = true;
                refresh_counter = 0;
            }
        }
        
        // 处理用户输入
        let key = get_char();
        match key {
            0 => {
                // 没有输入，短暂休眠避免CPU占用过高
                sleep(50);
                continue;
            },
            b'\n' | b'\r' | b' ' => {
                // Enter或空格键：手动刷新
                should_refresh = true;
                refresh_counter = 0; // 重置自动刷新计数器
            },
            b'a' | b'A' => {
                // 切换自动刷新
                auto_refresh = !auto_refresh;
                should_refresh = true;
                refresh_counter = 0; // 重置计数器
            },
            b'c' | b'C' => {
                // 按CPU排序
                sort_by = SortBy::Cpu;
                should_refresh = true;
                refresh_counter = 0;
            },
            b'm' | b'M' => {
                // 按内存排序
                sort_by = SortBy::Memory;
                should_refresh = true;
                refresh_counter = 0;
            },
            b'p' | b'P' => {
                // 按PID排序
                sort_by = SortBy::Pid;
                should_refresh = true;
                refresh_counter = 0;
            },
            b'v' | b'V' => {
                // 按VRuntime排序
                sort_by = SortBy::VRuntime;
                should_refresh = true;
                refresh_counter = 0;
            },
            b's' | b'S' => {
                // 按状态排序
                sort_by = SortBy::Status;
                should_refresh = true;
                refresh_counter = 0;
            },
            b'r' | b'R' => {
                // 反转排序
                reverse = !reverse;
                should_refresh = true;
                refresh_counter = 0;
            },
            b'q' | b'Q' => {
                // 退出
                println!("");
                println!("Exiting top command...");
                break;
            },
            b'h' | b'H' => {
                // 显示帮助
                println!("");
                show_help();
                println!("");
                println!("Press any key to continue...");
                let _ = get_char();
                should_refresh = true;
                refresh_counter = 0;
            },
            3 => {
                // Ctrl+C (ASCII 3)
                println!("");
                println!("Received Ctrl+C, exiting...");
                break;
            },
            _ => {
                // 其他键，显示帮助
                println!("");
                println!("Unknown key. Valid commands:");
                println!("  [Enter/Space] = Manual refresh, [a] = Toggle auto-refresh");
                println!("  [c] = Sort by CPU%, [m] = Memory, [p] = PID, [v] = VRuntime, [s] = Status");
                println!("  [r] = Reverse, [q] = Quit");
                sleep(1000);
            }
        }
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
    println!("  UID: {}, GID: {}, EUID: {}, EGID: {}", current_uid, current_gid, current_euid, current_egid);
    println!("  Heap: {} bytes", heap_size);
    println!("");
    println!("Note: Enhanced process monitoring requires kernel support.");
    println!("New system calls may not be available yet.");
}

// 单次显示模式
fn single_display() {
    display_header();
    display_system_stats();
    
    match display_all_processes() {
        Ok(()) => {
            println!("");
            println!("Single display completed successfully.");
        },
        Err(e) => {
            println!("Error: {}", e);
            println!("Falling back to basic display...");
            display_basic_info();
        }
    }
}

// 显示帮助信息
fn show_help() {
    println!("LiteOS Top Command - Advanced Process Monitor v3.0");
    println!("Usage: top [options]");
    println!("");
    println!("Options:");
    println!("  -h, --help     Show this help message");
    println!("  -1, --once     Display once and exit (non-interactive)");
    println!("  (no args)      Interactive mode with manual refresh and sorting");
    println!("");
    println!("Features:");
    println!("  - Real-time process monitoring with CPU usage tracking");
    println!("  - Interactive sorting by CPU%, Memory, PID, VRuntime, Status");
    println!("  - System statistics (CPU, memory, process counts)");
    println!("  - Manual refresh control (no auto-refresh)");
    println!("  - Proper Ctrl+C support for clean exit");
    println!("");
    println!("Display Columns:");
    println!("  PID   - Process ID");
    println!("  PPID  - Parent Process ID");
    println!("  UID   - User ID");
    println!("  GID   - Group ID");
    println!("  EUID  - Effective User ID");
    println!("  EGID  - Effective Group ID");
    println!("  STAT  - Process Status (READY/RUN/SLEEP/ZOMB)");
    println!("  PRI   - Priority (0-139, lower = higher priority)");
    println!("  NICE  - Nice value (-20 to 19)");
    println!("  %CPU  - CPU usage percentage");
    println!("  VRUN  - Virtual runtime (scheduling info)");
    println!("  HEAP  - Heap memory usage");
    println!("  STATUS- Last runtime information");
    println!("");
    println!("Interactive Commands:");
    println!("  [Enter/Space] - Manual refresh display");
    println!("  [a] - Toggle auto-refresh (default: ON)");
    println!("  [c] - Sort by CPU usage (default)");
    println!("  [m] - Sort by Memory usage");
    println!("  [p] - Sort by Process ID");
    println!("  [v] - Sort by Virtual runtime");
    println!("  [s] - Sort by Status");
    println!("  [r] - Reverse sort order");
    println!("  [q] - Quit");
    println!("  [Ctrl+C] - Force quit");
    println!("");
    println!("System Calls:");
    println!("  Enhanced with CPU usage tracking:");
    println!("  - sys_get_process_list (700) - Get all process IDs");
    println!("  - sys_get_process_info (701) - Get detailed process info with CPU%");
    println!("  - sys_get_system_stats (702) - Get system stats with CPU breakdown");
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("LiteOS Advanced Top Command v3.0 Starting...");
    println!("");
    
    // 显示功能介绍
    println!("Enhanced Process Monitor with CPU Tracking & Auto-Refresh");
    println!("Features:");
    println!("  ✓ CPU usage percentage tracking");
    println!("  ✓ Interactive sorting (CPU%, Memory, PID, VRuntime, Status)");
    println!("  ✓ Auto-refresh with manual control");
    println!("  ✓ Proper keyboard input handling");
    println!("  ✓ System statistics with CPU breakdown");
    println!("  ✓ Real memory statistics");
    println!("");
    
    // 测试系统调用可用性
    println!("Testing enhanced system calls...");
    let process_count = get_process_count();
    if process_count > 0 {
        println!("✓ Enhanced system calls are available!");
        println!("✓ Found {} processes in the system", process_count);
        println!("✓ CPU usage tracking enabled");
        println!("");
        
        // 显示快速帮助
        println!("Quick Help:");
        println!("  Interactive mode with auto-refresh and keyboard controls");
        println!("  Use [a] to toggle auto-refresh, [Enter] for manual refresh");
        println!("  Use [c/m/p/v/s] to sort, [r] to reverse, [q] to quit");
        println!("  Press [h] during runtime for full help, or restart with --help");
        println!("");
        
        interactive_mode();
    } else {
        println!("⚠ Enhanced system calls not available.");
        println!("This indicates that the kernel system calls haven't been loaded yet.");
        println!("");
        println!("Falling back to basic mode...");
        println!("");
        
        // 运行基本模式
        display_header();
        display_basic_info();
        
        println!("");
        println!("To get full functionality:");
        println!("1. Make sure the kernel has been rebuilt with the new system calls");
        println!("2. Restart the LiteOS system");
        println!("3. Run 'top' again");
        println!("");
        println!("Expected system calls:");
        println!("  - sys_get_process_list (700)");
        println!("  - sys_get_process_info (701)");  
        println!("  - sys_get_system_stats (702)");
    }
    
    println!("");
    println!("Top command completed.");
    0
}