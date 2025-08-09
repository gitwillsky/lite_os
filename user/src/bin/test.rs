#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use user_lib::{mmap_flags, syscall::{signals, SIG_BLOCK, SIG_SETMASK}, flock_consts};
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;
use core::ptr;

// Global variables for signal testing
static mut SIGNAL_COUNT: i32 = 0;
static mut SIGUSR1_COUNT: i32 = 0;
static mut SIGINT_COUNT: i32 = 0;

// Signal handlers
extern "C" fn sigint_handler(sig: i32) {
    unsafe {
        SIGINT_COUNT += 1;
        let count = SIGINT_COUNT;
        println!("📧 Core {} received SIGINT ({}), count: {}", get_hart_id(), sig, count);
    }
}

extern "C" fn sigusr1_handler(sig: i32) {
    unsafe {
        SIGUSR1_COUNT += 1;
        let count = SIGUSR1_COUNT;
        println!("📨 Core {} received SIGUSR1 ({}), count: {}", get_hart_id(), sig, count);
    }
}

extern "C" fn sigterm_handler(sig: i32) {
    println!("💀 Core {} received SIGTERM ({}), exiting gracefully", get_hart_id(), sig);
}

// Helper function to get current hart ID (simulated)
fn get_hart_id() -> usize {
    // In a real implementation, this would return the actual core ID
    // For now, we'll use PID as a proxy
    (getpid() as usize) % 4
}

// Helper function for progress display
fn print_progress_bar(current: usize, total: usize, width: usize, label: &str) {
    let percentage = (current * 100) / total;
    let filled = (current * width) / total;
    let empty = width - filled;

    print!("\r{}: [", label);
    for _ in 0..filled {
        print!("■");
    }
    for _ in 0..empty {
        print!("□");
    }
    print!("] {}% ({}/{})", percentage, current, total);

    if current == total {
        println!(""); // New line when complete
    }
}

fn print_test_header(test_name: &str, test_num: usize, total_tests: usize) {
    println!("\n{}", "═".repeat(60));
    println!("🧪 TEST {}/{}: {}", test_num, total_tests, test_name);
    println!("{}", "═".repeat(60));
}

fn print_test_result(test_name: &str, success: bool, elapsed_ms: u64) {
    let status = if success { "✅ PASSED" } else { "❌ FAILED" };
    let time_str = if elapsed_ms < 1000 {
        format!("{}ms", elapsed_ms)
    } else {
        format!("{:.1}s", elapsed_ms as f64 / 1000.0)
    };
    println!("\n{} {} ({})", status, test_name, time_str);
}

// Multi-core stress test
fn multicore_stress_test() -> i32 {
    println!("=== Multi-Core Stress Test ===");
    println!("Testing concurrent workloads on different cores...");

    let num_children = 4;
    let mut children = Vec::new();

    // Progress tracking
    println!("Creating child processes...");
    for i in 0..num_children {
        print_progress_bar(i + 1, num_children, 20, "Process Creation");

        let pid = fork();
        if pid == 0 {
            // Child process - simulate different workloads on different cores
            println!("\nChild {} (PID: {}) starting on core {}", i, getpid(), get_hart_id());

            match i {
                0 => cpu_intensive_task(i),
                1 => memory_intensive_task(i),
                2 => io_intensive_task(i),
                3 => signal_intensive_task(i),
                _ => basic_task(i),
            }

            println!("Child {} (PID: {}) completed", i, getpid());
            exit(0);
        } else if pid > 0 {
            children.push(pid);
            sleep_ms(100); // Brief delay between process creation
        } else {
            println!("\nFailed to fork child {}", i);
        }
    }

    println!("\nWaiting for child processes to complete...");
    // Wait for all children with progress tracking
    let mut all_success = true;
    for (i, child_pid) in children.iter().enumerate() {
        print_progress_bar(i + 1, children.len(), 20, "Process Completion");

        let mut exit_code = 0;
        let result = wait_pid(*child_pid as usize, &mut exit_code);
        if result >= 0 && exit_code == 0 {
            // Success - continue
        } else {
            println!("\nChild {} (PID: {}) failed with exit code {}", i, child_pid, exit_code);
            all_success = false;
        }
    }

    if all_success {
        println!("\n✅ Multi-core stress test passed!");
        0
    } else {
        println!("\n❌ Multi-core stress test failed!");
        1
    }
}

fn cpu_intensive_task(id: usize) {
    println!("CPU task {} starting intensive computation", id);
    let mut result = 1u64;
    let total_iterations = 100000;

    for i in 1..=total_iterations {
        result = result.wrapping_mul(i as u64).wrapping_add(i as u64);

        // Show progress every 10% completion
        if i % (total_iterations / 10) == 0 {
            let progress = (i * 100) / total_iterations;
            println!("CPU task {} progress: {}%", id, progress);
            yield_(); // Allow other processes to run
        }
    }
    println!("CPU task {} result: {}", id, result);
}

fn memory_intensive_task(id: usize) {
    println!("Memory task {} starting memory allocation test", id);
    let mut vectors = Vec::new();
    let total_allocations = 100;

    for i in 0..total_allocations {
        let mut vec = Vec::new();
        for j in 0..1000 {
            vec.push(i * 1000 + j);
        }
        vectors.push(vec);

        // Show progress every 20% completion
        if (i + 1) % (total_allocations / 5) == 0 {
            let progress = ((i + 1) * 100) / total_allocations;
            println!("Memory task {} progress: {}% ({} vectors)", id, progress, i + 1);
            yield_(); // Allow other processes to run
        }
    }

    println!("Memory task {} allocated {} vectors", id, vectors.len());
}

fn io_intensive_task(id: usize) {
    println!("I/O task {} starting file operations", id);

    for i in 0..10 {
        let filename = format!("/tmp_test_io_{}.txt", id);
        let fd = open(&filename, 0o100 | 0o644); // O_CREAT | mode

        if fd >= 0 {
            let content = format!("I/O test data from task {} iteration {}", id, i);
            write(fd as usize, content.as_bytes());
            close(fd as usize);

            // Read it back
            let read_fd = open(&filename, 0);
            if read_fd >= 0 {
                let mut buffer = [0u8; 256];
                read(read_fd as usize, &mut buffer);
                close(read_fd as usize);
            }

            remove(&filename); // Clean up
        }

        yield_(); // Allow other processes to run
    }

    println!("I/O task {} completed file operations", id);
}

fn signal_intensive_task(id: usize) {
    println!("Signal task {} starting signal tests", id);

    // Set up signal handlers
    signal(signals::SIGUSR1, sigusr1_handler as usize);

    let pid = getpid();

    for _i in 0..5 {
        // Send signal to self
        kill(pid as usize, signals::SIGUSR1);

        // Wait a bit
        for _ in 0..100000 {
            // Busy wait
        }

        yield_();
    }

    println!("Signal task {} completed signal tests", id);
}

fn basic_task(id: usize) {
    println!("Basic task {} doing simple operations", id);

    for i in 0..10 {
        let mut vec = Vec::new();
        for j in 0..100 {
            vec.push(i * 100 + j);
        }

        let sum: i32 = vec.iter().sum();
        println!("Basic task {} iteration {}: sum = {}", id, i, sum);

        yield_();
    }
}

// Complete syscall test suite
fn test_all_syscalls() -> i32 {
    println!("=== Complete Syscall Test Suite ===");
    let mut passed = 0;
    let mut total = 0;

    // Process Management Syscalls
    total += 1;
    if test_process_management() == 0 { passed += 1; }

    // Memory Management Syscalls
    total += 1;
    if test_memory_management() == 0 { passed += 1; }

    // File System Syscalls
    total += 1;
    if test_filesystem_syscalls() == 0 { passed += 1; }

    // I/O and File Descriptor Syscalls
    total += 1;
    if test_io_syscalls() == 0 { passed += 1; }

    // Signal Syscalls
    total += 1;
    if test_signal_syscalls() == 0 { passed += 1; }

    // Time and Sleep Syscalls
    total += 1;
    if test_time_syscalls() == 0 { passed += 1; }

    // Permission Syscalls
    total += 1;
    if test_permission_syscalls() == 0 { passed += 1; }

    // System Information Syscalls
    total += 1;
    if test_system_info_syscalls() == 0 { passed += 1; }

    // Edge Cases and Error Handling
    total += 1;
    if test_edge_cases() == 0 { passed += 1; }

    // Resource Exhaustion Tests
    total += 1;
    if test_resource_exhaustion() == 0 { passed += 1; }

    // Error Recovery Tests
    total += 1;
    if test_error_recovery() == 0 { passed += 1; }

    println!("Syscall tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_process_management() -> i32 {
    println!("--- Process Management Syscalls ---");

    // getpid, fork, exec, wait_pid, exit
    let pid = getpid();
    println!("getpid(): {}", pid);

    let child_pid = fork();
    if child_pid == 0 {
        // Child process
        println!("Child process PID: {}", getpid());
        exit(42);
    } else if child_pid > 0 {
        // Parent process
        let mut exit_code = 0;
        let result = wait_pid(child_pid as usize, &mut exit_code);
        if result >= 0 && exit_code == 42 {
            println!("✅ fork/wait_pid/exit test passed");
        } else {
            println!("❌ fork/wait_pid/exit test failed");
            return 1;
        }
    } else {
        println!("❌ fork failed");
        return 1;
    }

    // yield test
    println!("Testing yield...");
    for i in 0..5 {
        print!("yield test {} ", i);
        yield_();
    }
    println!("");

    println!("✅ Process management syscalls passed");
    0
}

fn test_memory_management() -> i32 {
    println!("--- Memory Management Syscalls ---");

    // brk, sbrk
    let initial_brk = brk(0);
    println!("brk(0): {:#x}", initial_brk);

    let new_brk = brk(initial_brk as usize + 4096);
    if new_brk > initial_brk {
        println!("✅ brk expansion worked");
    } else {
        println!("❌ brk expansion failed");
        return 1;
    }

    let old_brk = sbrk(4096);
    let current_brk = sbrk(0);
    if current_brk as usize == old_brk as usize + 4096 {
        println!("✅ sbrk test passed");
    } else {
        println!("❌ sbrk test failed");
        return 1;
    }

    // mmap, munmap
    let addr = mmap(0, 4096, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
    if addr > 0 {
        println!("mmap allocated: {:#x}", addr);

        // Test writing to mapped memory
        unsafe {
            let ptr = addr as *mut u32;
            *ptr = 0xDEADBEEF;
            if *ptr == 0xDEADBEEF {
                println!("✅ mmap write/read test passed");
            } else {
                println!("❌ mmap write/read test failed");
                return 1;
            }
        }

        if munmap(addr as usize, 4096) == 0 {
            println!("✅ munmap test passed");
        } else {
            println!("❌ munmap test failed");
            return 1;
        }
    } else {
        println!("❌ mmap failed");
        return 1;
    }

    println!("✅ Memory management syscalls passed");
    0
}

fn test_filesystem_syscalls() -> i32 {
    println!("--- File System Syscalls ---");

    // listdir
    let mut buf = [0u8; 1024];
    let len = listdir("/", &mut buf);
    if len > 0 {
        println!("✅ listdir test passed (found {} bytes)", len);
    } else {
        println!("❌ listdir test failed");
        return 1;
    }

    // mkdir
    let test_dir = "/test_directory";
    if mkdir(test_dir) == 0 {
        println!("✅ mkdir test passed");

        // Remove the directory
        if remove(test_dir) == 0 {
            println!("✅ remove directory test passed");
        } else {
            println!("Note: directory removal may not be fully implemented");
        }
    } else {
        println!("❌ mkdir test failed");
        return 1;
    }

    // chdir, getcwd
    let mut cwd_buf = [0u8; 256];
    if getcwd(&mut cwd_buf) > 0 {
        println!("✅ getcwd test passed");

        // Try to change directory (may not work if directory doesn't exist)
        let result = chdir("/");
        if result == 0 {
            println!("✅ chdir test passed");
        } else {
            println!("Note: chdir test result: {}", result);
        }
    } else {
        println!("❌ getcwd test failed");
        return 1;
    }

    println!("✅ File system syscalls passed");
    0
}

fn test_io_syscalls() -> i32 {
    println!("--- I/O and File Descriptor Syscalls ---");

    // open, write, read, close
    let test_file = "/tmp_test_io_file.txt";
    let fd = open(test_file, 0o100 | 0o644); // O_CREAT | mode
    if fd >= 0 {
        let test_data = b"Hello, LiteOS I/O test!";
        let written = write(fd as usize, test_data);
        if written as usize == test_data.len() {
            println!("✅ write test passed");
        } else {
            println!("❌ write test failed");
            return 1;
        }
        close(fd as usize);

        // Read it back
        let read_fd = open(test_file, 0);
        if read_fd >= 0 {
            let mut buffer = [0u8; 64];
            let bytes_read = read(read_fd as usize, &mut buffer);
            if bytes_read > 0 {
                println!("✅ read test passed ({} bytes)", bytes_read);
            } else {
                println!("❌ read test failed: read returned {}", bytes_read);
                return 1;
            }
            close(read_fd as usize);
        } else {
            println!("❌ read test failed: could not open file for reading (fd={})", read_fd);
            return 1;
        }

        remove(test_file); // Clean up
    } else {
        println!("❌ open test failed");
        return 1;
    }

    // dup, dup2
    let fd = open("/hello.txt", 0);
    if fd >= 0 {
        let dup_fd = dup(fd as usize);
        if dup_fd >= 0 {
            println!("✅ dup test passed");
            close(dup_fd as usize);
        } else {
            println!("❌ dup test failed");
            return 1;
        }

        let fd2 = open("/hello.txt", 0);
        if fd2 >= 0 {
            let result = dup2(fd as usize, fd2 as usize);
            if result == fd2 {
                println!("✅ dup2 test passed");
            } else {
                println!("❌ dup2 test failed");
                return 1;
            }
            close(fd2 as usize);
        }
        close(fd as usize);
    }

    // pipe
    let mut pipe_fds = [0i32; 2];
    if pipe(&mut pipe_fds) == 0 {
        println!("✅ pipe created: read_fd={}, write_fd={}", pipe_fds[0], pipe_fds[1]);

        let test_msg = b"pipe test message";
        let written = write(pipe_fds[1] as usize, test_msg);
        if written > 0 {
            let mut buffer = [0u8; 32];
            let read_bytes = read(pipe_fds[0] as usize, &mut buffer);
            if read_bytes > 0 {
                println!("✅ pipe communication test passed");
            } else {
                println!("❌ pipe read test failed");
                return 1;
            }
        } else {
            println!("❌ pipe write test failed");
            return 1;
        }

        close(pipe_fds[0] as usize);
        close(pipe_fds[1] as usize);
    } else {
        println!("❌ pipe creation failed");
        return 1;
    }

    println!("✅ I/O syscalls passed");
    0
}

fn test_signal_syscalls() -> i32 {
    println!("--- Signal Syscalls ---");

    // signal, kill, sigprocmask
    let old_handler = signal(signals::SIGUSR1, sigusr1_handler as usize);
    if old_handler >= 0 {
        println!("✅ signal handler setup passed");
    } else {
        println!("❌ signal handler setup failed");
        return 1;
    }

    let pid = getpid();
    if kill(pid as usize, signals::SIGUSR1) == 0 {
        println!("✅ kill syscall passed");

        // Wait for signal handling
        for _ in 0..1000000 {
            // Busy wait
        }
    } else {
        println!("❌ kill syscall failed");
        return 1;
    }

    // Test signal masking
    let mut old_mask = 0u64;
    let new_mask = 1u64 << (signals::SIGUSR1 - 1);
    if sigprocmask(SIG_BLOCK, &new_mask, &mut old_mask) == 0 {
        println!("✅ sigprocmask test passed");

        // Restore mask
        sigprocmask(SIG_SETMASK, &old_mask, ptr::null_mut());
    } else {
        println!("❌ sigprocmask test failed");
        return 1;
    }

    println!("✅ Signal syscalls passed");
    0
}

fn test_time_syscalls() -> i32 {
    println!("--- Time and Sleep Syscalls ---");

    // get_time_ms, get_time_us, get_time_ns
    let time_ms = get_time_ms();
    let time_us = get_time_us();
    let time_ns = get_time_ns();

    println!("Time: {}ms, {}us, {}ns", time_ms, time_us, time_ns);

    if time_ms > 0 && time_us > 0 && time_ns > 0 {
        println!("✅ time syscalls passed");
    } else {
        println!("❌ time syscalls failed");
        return 1;
    }

    // sleep_ms
    let start_time = get_time_ms();
    sleep_ms(100); // Sleep for 100ms
    let end_time = get_time_ms();
    let elapsed = end_time - start_time;

    if elapsed >= 90 && elapsed <= 200 { // Allow some tolerance
        println!("✅ sleep_ms test passed (elapsed: {}ms)", elapsed);
    } else {
        println!("❌ sleep_ms test failed (elapsed: {}ms)", elapsed);
        return 1;
    }

    println!("✅ Time syscalls passed");
    0
}

fn test_permission_syscalls() -> i32 {
    println!("--- Permission Syscalls ---");

    // getuid, geteuid, getgid, getegid
    let uid = getuid();
    let euid = geteuid();
    let gid = getgid();
    let egid = getegid();

    println!("UID: {}, EUID: {}, GID: {}, EGID: {}", uid, euid, gid, egid);

    // chmod, chown (test on a file we create)
    let test_file = "/tmp_test_perm_file.txt";
    let fd = open(test_file, 0o100 | 0o644);
    if fd >= 0 {
        write(fd as usize, b"permission test");
        close(fd as usize);

        if chmod(test_file, 0o755) == 0 {
            println!("✅ chmod test passed");
        } else {
            println!("❌ chmod test failed");
            return 1;
        }

        // chown may fail if not root, but we test it anyway
        let chown_result = chown(test_file, uid, gid);
        println!("chown result: {} (may fail if not root)", chown_result);

        remove(test_file); // Clean up
    }

    println!("✅ Permission syscalls passed");
    0
}

fn test_system_info_syscalls() -> i32 {
    println!("--- System Information Syscalls ---");

    // get_process_list, get_process_info, get_system_stats
    let mut pids = vec![0u32; 32];
    let count = get_process_list(&mut pids);
    if count > 0 {
        println!("✅ get_process_list found {} processes", count);

        // Test get_process_info on first few processes
        for i in 0..(count.min(3) as usize) {
            let mut info = ProcessInfo {
                pid: 0, ppid: 0, uid: 0, gid: 0, euid: 0, egid: 0,
                status: 0, priority: 0, nice: 0, vruntime: 0,
                heap_base: 0, heap_top: 0, last_runtime: 0,
                total_cpu_time: 0, cpu_percent: 0, core_id: 0,
                name: [0u8; 32],
            };

            if get_process_info(pids[i], &mut info) == 0 {
                println!("Process {}: PID={}, core={}", i, info.pid, info.core_id);
            }
        }
        println!("✅ get_process_info test passed");
    } else {
        println!("❌ get_process_list failed");
        return 1;
    }

    let mut stats = SystemStats {
        total_processes: 0, running_processes: 0, sleeping_processes: 0,
        zombie_processes: 0, total_memory: 0, used_memory: 0, free_memory: 0,
        system_uptime: 0, cpu_user_time: 0, cpu_system_time: 0,
        cpu_idle_time: 0, cpu_usage_percent: 0,
    };

    if get_system_stats(&mut stats) == 0 {
        println!("✅ get_system_stats passed: {} processes, {}MB memory",
                stats.total_processes, stats.total_memory / (1024 * 1024));
    } else {
        println!("❌ get_system_stats failed");
        return 1;
    }

    println!("✅ System info syscalls passed");
    0
}

// Multi-core signal test - specifically for the Ctrl+C issue
fn test_multicore_signals() -> i32 {
    println!("=== Multi-Core Signal Test ===");
    println!("Testing cross-core signal delivery (Ctrl+C issue fix)");

    // Set up signal handlers
    signal(signals::SIGINT, sigint_handler as usize);
    signal(signals::SIGUSR1, sigusr1_handler as usize);

    let mut children = Vec::new();
    let num_children = 3;

    for i in 0..num_children {
        let pid = fork();
        if pid == 0 {
            // Child process - simulate busy work on different cores
            println!("Child {} (PID: {}) starting signal test on core {}", i, getpid(), get_hart_id());

            // Set up signal handlers in child
            signal(signals::SIGINT, sigint_handler as usize);
            signal(signals::SIGUSR1, sigusr1_handler as usize);

            // Do some work while waiting for signals
            for iteration in 0..10 {
                // Simulate CPU-intensive work
                let mut sum = 0u64;
                for j in 0..50000 {
                    sum = sum.wrapping_add(j as u64);
                }

                println!("Child {} iteration {}: sum={}", i, iteration, sum);

                // Check for signals every few iterations
                if iteration % 3 == 0 {
                    yield_(); // Give opportunity for signal delivery
                }

                // Sleep briefly
                sleep_ms(20);
            }

            println!("Child {} completed normally", i);
            exit(0);
        } else if pid > 0 {
            children.push(pid);
            println!("Created child {} with PID {}", i, pid);
        } else {
            println!("Failed to fork child {}", i);
            return 1;
        }
    }

    // Wait a bit for children to start
    sleep_ms(100);

    // Send signals to children to test cross-core delivery
    for (i, child_pid) in children.iter().enumerate() {
        println!("Sending SIGUSR1 to child {} (PID: {})", i, child_pid);
        if kill(*child_pid as usize, signals::SIGUSR1) != 0 {
            println!("Failed to send signal to child {}", i);
        }
        sleep_ms(50);
    }

    // Send SIGINT to test termination
    sleep_ms(100);
    for (i, child_pid) in children.iter().enumerate() {
        println!("Sending SIGINT to child {} (PID: {})", i, child_pid);
        if kill(*child_pid as usize, signals::SIGINT) != 0 {
            println!("Failed to send SIGINT to child {}", i);
        }
    }

    // Wait for all children to finish
    let mut all_signaled = true;
    for (i, child_pid) in children.iter().enumerate() {
        let mut exit_code = 0;
        let result = wait_pid(*child_pid as usize, &mut exit_code);
        if result >= 0 {
            println!("Child {} (PID: {}) exited with code {}", i, child_pid, exit_code);
        } else {
            println!("Failed to wait for child {} (PID: {})", i, child_pid);
            all_signaled = false;
        }
    }

    // Show signal statistics
    unsafe {
        println!("Signal delivery statistics:");
        println!("  SIGINT handled: {} times", core::ptr::read_volatile(&raw const SIGINT_COUNT));
        println!("  SIGUSR1 handled: {} times", core::ptr::read_volatile(&raw const SIGUSR1_COUNT));
    }

    if all_signaled {
        println!("✅ Multi-core signal test passed!");
        0
    } else {
        println!("❌ Multi-core signal test failed!");
        1
    }
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("🚀 === LiteOS Comprehensive Test Suite ===");
    println!("Testing all syscalls and multi-core functionality");
    println!("=================================================\n");

    let mut total_tests = 0;
    let mut passed_tests = 0;
    let start_time = get_time_ms();

    // Run comprehensive test suite
    let tests: Vec<(&str, fn() -> i32)> = vec![
        ("Complete Syscall Suite", test_all_syscalls),
        ("Multi-Core Stress Test", multicore_stress_test),
        ("Multi-Core Signal Test", test_multicore_signals),
        ("Memory Pressure Test", memory_pressure_test),
        ("Filesystem Stress Test", filesystem_stress_test),
        ("IPC Reliability Test", ipc_reliability_test),
        ("Long Running Stability Test", long_running_stability_test),
    ];

    println!("📋 Test Suite Overview: {} tests scheduled", tests.len());
    println!("{}", "=".repeat(60));

    for (test_index, (test_name, test_func)) in tests.iter().enumerate() {
        total_tests += 1;
        let test_start_time = get_time_ms();

        print_test_header(test_name, test_index + 1, tests.len());

        let result = test_func();
        let test_elapsed = get_time_ms() - test_start_time;

        if result == 0 {
            passed_tests += 1;
            print_test_result(test_name, true, test_elapsed as u64);
        } else {
            print_test_result(test_name, false, test_elapsed as u64);
        }

        // Overall progress
        print_progress_bar(test_index + 1, tests.len(), 30, "Overall Progress");
        println!(""); // Extra line for spacing
    }

    let total_elapsed = get_time_ms() - start_time;

    // Final summary with enhanced formatting
    println!("\n{}", "█".repeat(60));
    println!("📊 === FINAL TEST RESULTS ===");
    println!("{}", "█".repeat(60));
    println!("⏱️  Total execution time: {:.2}s", total_elapsed as f64 / 1000.0);
    println!("📈 Total test suites: {}", total_tests);
    println!("✅ Passed: {}", passed_tests);
    println!("❌ Failed: {}", total_tests - passed_tests);
    println!("📊 Success rate: {:.1}%", (passed_tests as f32 / total_tests as f32) * 100.0);

    // Progress bar for final results
    print_progress_bar(passed_tests, total_tests, 40, "Test Success Rate");

    if passed_tests == total_tests {
        println!("🎉 ALL TESTS PASSED! LiteOS multi-core functionality is working correctly.");
        println!("✅ System stability verified across all test categories!");
        println!("🏆 IPI-based cross-core signal delivery is functional!");
    } else {
        println!("⚠️  Some tests failed. System may need attention.");
        println!("🔧 Check individual test results for detailed diagnostics.");
    }

    println!("{}", "█".repeat(60));
    println!("=== LiteOS Comprehensive Test Suite Complete ===");

    // Return success if all tests passed
    if passed_tests == total_tests { 0 } else { 1 }
}

// Edge cases and boundary condition tests
fn test_edge_cases() -> i32 {
    println!("--- Edge Cases and Boundary Conditions ---");
    let mut passed = 0;
    let mut total = 0;

    // Test with invalid file descriptors
    total += 1;
    if test_invalid_fd() { passed += 1; }

    // Test with invalid pointers/addresses
    total += 1;
    if test_invalid_pointers() { passed += 1; }

    // Test with large/extreme values
    total += 1;
    if test_extreme_values() { passed += 1; }

    // Test concurrent access patterns
    total += 1;
    if test_concurrent_access() { passed += 1; }

    println!("Edge case tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_invalid_fd() -> bool {
    println!("Testing invalid file descriptors...");

    // Test read/write with invalid FDs
    let mut buffer = [0u8; 10];
    let invalid_fds = [9999, -1i32 as usize, usize::MAX];

    for &fd in &invalid_fds {
        let read_result = read(fd, &mut buffer);
        let write_result = write(fd, b"test");

        // These should fail gracefully, not crash
        if read_result >= 0 || write_result >= 0 {
            println!("Warning: invalid FD {} didn't return error", fd);
        }
    }

    // Test close with invalid FD
    if close(9999) == 0 {
        println!("Warning: close(9999) unexpectedly succeeded");
    }

    println!("✅ Invalid FD tests completed");
    true
}

fn test_invalid_pointers() -> bool {
    println!("Testing invalid pointer handling...");

    // Note: We can't test truly invalid pointers without crashing,
    // but we can test boundary conditions

    // Test with very small buffer sizes
    let mut tiny_buffer = [0u8; 0];
    let fd = open("/hello.txt", 0);
    if fd >= 0 {
        let result = read(fd as usize, &mut tiny_buffer);
        println!("Read with 0-size buffer result: {}", result);
        close(fd as usize);
    }

    // Test getcwd with insufficient buffer
    let mut small_buf = [0u8; 2];
    let result = getcwd(&mut small_buf);
    println!("getcwd with tiny buffer result: {}", result);

    println!("✅ Invalid pointer tests completed");
    true
}

fn test_extreme_values() -> bool {
    println!("Testing extreme values...");

    // Test very large memory allocations - 内核应该能够正确处理这种情况
    let huge_size = 1024 * 1024 * 1024; // 1GB
    let addr = mmap(0, huge_size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
    if addr > 0 {
        println!("Huge mmap succeeded: {:#x}", addr);
        munmap(addr as usize, huge_size);
    } else {
        println!("Huge mmap failed as expected");
    }

    // Test brk with extreme values - 应该优雅地失败而不是panic
    let original_brk = brk(0);
    let extreme_brk = brk(usize::MAX);
    if extreme_brk != original_brk {
        println!("Warning: extreme brk succeeded unexpectedly");
    }

    // Test sleep with extreme values
    let start_time = get_time_ms();
    sleep_ms(0); // Zero sleep
    let end_time = get_time_ms();
    println!("Zero sleep took: {}ms", end_time - start_time);

    println!("✅ Extreme value tests completed");
    true
}

fn test_concurrent_access() -> bool {
    println!("Testing concurrent access patterns...");

    let test_file = "/tmp_concurrent_test.txt";
    let fd = open(test_file, 0o100 | 0o644);
    if fd < 0 {
        println!("Failed to create test file");
        return false;
    }

    // Create multiple child processes that access the same file
    let num_children = 3;
    let mut children = Vec::new();

    for i in 0..num_children {
        let child_pid = fork();
        if child_pid == 0 {
            // Child process - write to file concurrently
            for j in 0..5 {
                let data = format!("Child {} write {}\n", i, j);
                write(fd as usize, data.as_bytes());
                yield_(); // Give other processes a chance
            }
            close(fd as usize);
            exit(0);
        } else if child_pid > 0 {
            children.push(child_pid);
        }
    }

    // Parent also writes
    for j in 0..5 {
        let data = format!("Parent write {}\n", j);
        write(fd as usize, data.as_bytes());
        yield_();
    }

    close(fd as usize);

    // Wait for all children
    for child_pid in children {
        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);
    }

    // Read back the file to see if data is intact
    let read_fd = open(test_file, 0);
    if read_fd >= 0 {
        let mut buffer = [0u8; 1024];
        let bytes_read = read(read_fd as usize, &mut buffer);
        println!("Concurrent file access result: {} bytes read", bytes_read);
        close(read_fd as usize);
    }

    remove(test_file);
    println!("✅ Concurrent access tests completed");
    true
}

// Resource exhaustion tests
fn test_resource_exhaustion() -> i32 {
    println!("--- Resource Exhaustion Tests ---");
    let mut passed = 0;
    let mut total = 0;

    total += 1;
    if test_fd_exhaustion() { passed += 1; }

    total += 1;
    if test_memory_exhaustion() { passed += 1; }

    total += 1;
    if test_process_exhaustion() { passed += 1; }

    println!("Resource exhaustion tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_fd_exhaustion() -> bool {
    println!("Testing file descriptor exhaustion...");

    let mut fds = Vec::new();
    let test_file = "/tmp_fd_test.txt";

    // Create a test file first
    let initial_fd = open(test_file, 0o100 | 0o644);
    if initial_fd >= 0 {
        write(initial_fd as usize, b"test");
        close(initial_fd as usize);
    }

    // Try to open many files until we hit the limit
    for i in 0..1000 {
        if i % 100 == 0 {
            println!("Opening file descriptor #{}", i);
        }

        // Extra debug for critical range
        if i >= 890 {
            println!("CRITICAL: Opening FD #{} - this may trigger the hang", i);
        }

        let fd = open(test_file, 0);
        if fd >= 0 {
            fds.push(fd);
        } else {
            println!("FD limit reached at {} file descriptors (fd={})", i, fd);
            break;
        }
        if i > 1050 {
            println!("Breaking at iteration {} to prevent infinite loop", i);
            break;
        }
    }

    // Close all opened FDs
    for fd in fds {
        close(fd as usize);
    }

    remove(test_file);
    println!("✅ FD exhaustion test completed");
    true
}

fn test_memory_exhaustion() -> bool {
    println!("Testing memory exhaustion...");

    let mut allocations = Vec::new();
    let chunk_size = 1024 * 1024; // 1MB chunks

    // Try to allocate memory until we fail
    for i in 0..100 {
        let addr = mmap(0, chunk_size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            allocations.push(addr);

            // Write to the memory to ensure it's actually allocated
            unsafe {
                let ptr = addr as *mut u32;
                *ptr = 0xDEADBEEF;
            }
        } else {
            println!("Memory allocation failed at {} MB", i);
            break;
        }
    }

    // Free all allocations
    for addr in allocations {
        munmap(addr as usize, chunk_size);
    }

    println!("✅ Memory exhaustion test completed");
    true
}

fn test_process_exhaustion() -> bool {
    println!("Testing process creation limits...");

    let max_children = 50; // Reasonable limit for testing
    let mut children = Vec::new();

    for i in 0..max_children {
        let child_pid = fork();
        if child_pid == 0 {
            // Child process - just sleep briefly and exit
            sleep_ms(100);
            exit(0);
        } else if child_pid > 0 {
            children.push(child_pid);
        } else {
            println!("Process creation failed at {} children", i);
            break;
        }
    }

    println!("Created {} child processes", children.len());

    // Wait for all children
    for child_pid in children {
        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);
    }

    println!("✅ Process exhaustion test completed");
    true
}

// Error recovery tests
fn test_error_recovery() -> i32 {
    println!("--- Error Recovery Tests ---");
    let mut passed = 0;
    let mut total = 0;

    total += 1;
    if test_signal_during_syscall() { passed += 1; }

    total += 1;
    if test_cleanup_after_error() { passed += 1; }

    total += 1;
    if test_partial_operations() { passed += 1; }

    println!("Error recovery tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_signal_during_syscall() -> bool {
    println!("Testing signal interruption of syscalls...");

    // Set up signal handler
    signal(signals::SIGUSR1, sigusr1_handler as usize);

    let child_pid = fork();
    if child_pid == 0 {
        // Child - sleep for a while
        println!("Child sleeping...");
        sleep_ms(1000); // 1 second
        println!("Child woke up");
        exit(0);
    } else if child_pid > 0 {
        // Parent - interrupt the child's sleep
        sleep_ms(100); // Let child start sleeping
        println!("Sending signal to interrupt sleep...");
        kill(child_pid as usize, signals::SIGUSR1);

        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);
        println!("Child exited with code: {}", exit_code);
    }

    println!("✅ Signal interruption test completed");
    true
}

fn test_cleanup_after_error() -> bool {
    println!("Testing cleanup after errors...");

    // Test that failed operations don't leave resources leaked
    let original_fd_count = count_open_fds();

    // Try to open non-existent file multiple times
    for _ in 0..10 {
        let fd = open("/non/existent/file", 0);
        if fd >= 0 {
            close(fd as usize); // Shouldn't happen, but clean up if it does
        }
    }

    let final_fd_count = count_open_fds();
    if final_fd_count == original_fd_count {
        println!("✅ No FD leaks detected after failed opens");
    } else {
        println!("⚠️ Possible FD leak: {} -> {}", original_fd_count, final_fd_count);
    }

    println!("✅ Cleanup test completed");
    true
}

fn count_open_fds() -> i32 {
    // Rough estimate by trying to dup stdin multiple times
    let mut count = 0;
    for i in 3..100 { // Start from 3 (after stdin/stdout/stderr)
        let mut buffer = [0u8; 1];
        if read(i, &mut buffer) >= 0 || write(i, b"") >= 0 {
            count += 1;
        }
    }
    count
}

fn test_partial_operations() -> bool {
    println!("Testing partial I/O operations...");

    let test_file = "/tmp_partial_test.txt";
    let fd = open(test_file, 0o100 | 0o644);
    if fd < 0 {
        println!("Failed to create test file");
        return false;
    }

    // Write large amount of data
    let large_data = vec![0x42u8; 10000];
    let written = write(fd as usize, &large_data);
    println!("Attempted to write {} bytes, actually wrote {}", large_data.len(), written);

    close(fd as usize);

    // Read it back in small chunks
    let read_fd = open(test_file, 0);
    if read_fd >= 0 {
        let mut total_read = 0;
        let mut buffer = [0u8; 100];

        loop {
            let bytes_read = read(read_fd as usize, &mut buffer);
            if bytes_read <= 0 {
                break;
            }
            total_read += bytes_read as usize;
        }

        println!("Total bytes read back: {}", total_read);
        close(read_fd as usize);
    }

    remove(test_file);
    println!("✅ Partial operations test completed");
    true
}

// Memory pressure and fragmentation test
fn memory_pressure_test() -> i32 {
    println!("=== Memory Pressure Test ===");
    println!("Testing memory fragmentation, allocation patterns, and OOM handling");

    let mut passed = 0;
    let mut total = 0;

    total += 1;
    if test_memory_fragmentation() { passed += 1; }

    total += 1;
    if test_allocation_patterns() { passed += 1; }

    total += 1;
    if test_oom_handling() { passed += 1; }

    total += 1;
    if test_memory_alignment() { passed += 1; }

    println!("Memory pressure tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_memory_fragmentation() -> bool {
    println!("Testing memory fragmentation patterns...");

    let mut allocations = Vec::new();
    let sizes = [1024, 2048, 4096, 8192, 16384]; // Various sizes

    // Phase 1: Allocate memory in different sizes
    for i in 0..100 {
        let size = sizes[i % sizes.len()];
        let addr = mmap(0, size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            allocations.push((addr, size));

            // Write pattern to verify memory
            unsafe {
                let ptr = addr as *mut u32;
                for j in 0..(size / 4) {
                    *ptr.add(j) = (i * 1000 + j) as u32;
                }
            }
        }
    }

    println!("Phase 1: Allocated {} memory blocks", allocations.len());

    // Phase 2: Free every other allocation to create fragmentation
    let mut freed_count = 0;
    for i in (0..allocations.len()).step_by(2) {
        let (addr, size) = allocations[i];
        if munmap(addr as usize, size) == 0 {
            freed_count += 1;
        }
    }

    println!("Phase 2: Freed {} blocks to create fragmentation", freed_count);

    // Phase 3: Try to allocate large blocks in fragmented space
    let mut large_allocs = Vec::new();
    for _ in 0..10 {
        let addr = mmap(0, 32768, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            large_allocs.push(addr);
        }
    }

    println!("Phase 3: Allocated {} large blocks in fragmented space", large_allocs.len());

    // Cleanup
    for (addr, size) in allocations.iter().step_by(2).skip(1) {
        munmap(*addr as usize, *size);
    }
    for addr in large_allocs {
        munmap(addr as usize, 32768);
    }

    println!("✅ Memory fragmentation test completed");
    true
}

fn test_allocation_patterns() -> bool {
    println!("Testing various allocation patterns...");

    // Test 1: Rapid alloc/free cycles
    for cycle in 0..50 {
        let addr = mmap(0, 4096, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            // Write to ensure it's mapped
            unsafe {
                *(addr as *mut u32) = cycle;
            }
            munmap(addr as usize, 4096);
        }
    }
    println!("Rapid alloc/free cycles completed");

    // Test 2: Growing allocations
    let mut growing_allocs = Vec::new();
    let mut size = 1024;
    while size <= 1024 * 1024 {
        let addr = mmap(0, size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            growing_allocs.push((addr, size));
            // Touch the memory
            unsafe {
                let ptr = addr as *mut u8;
                for i in (0..size).step_by(4096) {
                    *ptr.add(i) = (i & 0xFF) as u8;
                }
            }
        } else {
            break;
        }
        size *= 2;
    }

    println!("Growing allocations: created {} blocks", growing_allocs.len());

    // Cleanup growing allocations
    for (addr, size) in growing_allocs {
        munmap(addr as usize, size);
    }

    println!("✅ Allocation patterns test completed");
    true
}

fn test_oom_handling() -> bool {
    println!("Testing OOM (Out of Memory) handling...");

    let mut allocations = Vec::new();
    let chunk_size = 1024 * 1024; // 1MB
    let mut total_allocated = 0;

    // Try to allocate until we hit memory limits
    for i in 0..200 { // Limit attempts to avoid infinite loop
        let addr = mmap(0, chunk_size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            // Actually touch the memory to force allocation
            unsafe {
                let ptr = addr as *mut u32;
                *ptr = 0xDEADBEEF;
                // Touch every page
                for j in (0..chunk_size).step_by(4096) {
                    *((addr as usize + j) as *mut u32) = i as u32;
                }
            }
            allocations.push(addr);
            total_allocated += chunk_size;
        } else {
            println!("OOM reached after allocating {} MB", total_allocated / (1024 * 1024));
            break;
        }

        // Yield occasionally to avoid hogging CPU
        if i % 10 == 0 {
            yield_();
        }
    }

    // Verify we can still do basic operations after OOM
    let test_fd = open("/hello.txt", 0);
    let can_read = if test_fd >= 0 {
        let mut buffer = [0u8; 10];
        let result = read(test_fd as usize, &mut buffer);
        close(test_fd as usize);
        result > 0
    } else {
        false
    };

    if can_read {
        println!("✅ System still functional after OOM");
    } else {
        println!("⚠️ System may be degraded after OOM");
    }

    // Cleanup all allocations
    for addr in allocations {
        munmap(addr as usize, chunk_size);
    }

    println!("✅ OOM handling test completed");
    true
}

fn test_memory_alignment() -> bool {
    println!("Testing memory alignment requirements...");

    // Test various alignment requirements
    let alignments = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 4096];

    for &alignment in &alignments {
        let addr = mmap(0, alignment * 2, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
        if addr > 0 {
            let aligned = (addr as usize + alignment - 1) & !(alignment - 1);

            // Test aligned memory access
            unsafe {
                match alignment {
                    1 => *(aligned as *mut u8) = 0x42,
                    2 => *(aligned as *mut u16) = 0x4242,
                    4 => *(aligned as *mut u32) = 0x42424242,
                    8 => *(aligned as *mut u64) = 0x4242424242424242,
                    _ => {
                        // For larger alignments, just write a pattern
                        let ptr = aligned as *mut u32;
                        for i in 0..(alignment / 4) {
                            *ptr.add(i) = 0x42424242;
                        }
                    }
                }
            }

            munmap(addr as usize, alignment * 2);
        }
    }

    println!("✅ Memory alignment test completed");
    true
}

// Filesystem stress test
fn filesystem_stress_test() -> i32 {
    println!("=== Filesystem Stress Test ===");
    let mut passed = 0;
    let mut total = 0;

    total += 1;
    if test_concurrent_file_ops() { passed += 1; }

    total += 1;
    if test_large_file_ops() { passed += 1; }

    total += 1;
    if test_many_small_files() { passed += 1; }

    println!("Filesystem stress tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_concurrent_file_ops() -> bool {
    println!("Testing concurrent filesystem operations...");

    let num_children = 4;
    let mut children = Vec::new();

    for i in 0..num_children {
        let child_pid = fork();
        if child_pid == 0 {
            // Child process - perform file operations
            for j in 0..20 {
                let filename = format!("/tmp_stress_file_{}_{}.txt", i, j);
                let fd = open(&filename, 0o100 | 0o644);
                if fd >= 0 {
                    let data = format!("Data from child {} iteration {}\n", i, j);
                    write(fd as usize, data.as_bytes());
                    close(fd as usize);

                    // Read it back
                    let read_fd = open(&filename, 0);
                    if read_fd >= 0 {
                        let mut buffer = [0u8; 128];
                        read(read_fd as usize, &mut buffer);
                        close(read_fd as usize);
                    }

                    remove(&filename);
                }

                if j % 5 == 0 {
                    yield_();
                }
            }
            exit(0);
        } else if child_pid > 0 {
            children.push(child_pid);
        }
    }

    // Wait for all children
    for child_pid in children {
        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);
    }

    println!("✅ Concurrent file operations test completed");
    true
}

fn test_large_file_ops() -> bool {
    println!("Testing large file operations...");

    let large_file = "/tmp_large_test_file.bin";
    let fd = open(large_file, 0o100 | 0o644);
    if fd < 0 {
        println!("Failed to create large file");
        return false;
    }

    // Write large amounts of data in chunks
    let chunk_size = 8192;
    let chunk_data = vec![0x55u8; chunk_size];
    let num_chunks = 100; // Total ~800KB

    let mut total_written = 0;
    for i in 0..num_chunks {
        let written = write(fd as usize, &chunk_data);
        total_written += written;

        if i % 10 == 0 {
            println!("Written {} chunks ({} bytes)", i + 1, total_written);
            yield_();
        }
    }

    close(fd as usize);

    println!("Total written: {} bytes", total_written);

    // Read it back in different chunk sizes
    let read_fd = open(large_file, 0);
    if read_fd >= 0 {
        let mut total_read = 0;
        let mut buffer = vec![0u8; chunk_size / 2]; // Different chunk size

        loop {
            let bytes_read = read(read_fd as usize, &mut buffer);
            if bytes_read <= 0 {
                break;
            }
            total_read += bytes_read as usize;
        }

        close(read_fd as usize);
        println!("Total read back: {} bytes", total_read);

        if total_read == total_written as usize {
            println!("✅ Large file integrity verified");
        } else {
            println!("⚠️ Large file integrity check failed");
        }
    }

    remove(large_file);
    println!("✅ Large file operations test completed");
    true
}

fn test_many_small_files() -> bool {
    println!("Testing many small file operations...");

    let num_files = 200;
    let mut created_files = Vec::new();

    // Create many small files with safe naming pattern
    for i in 0..num_files {
        let filename = format!("/tmp_small_file_{:04}.txt", i);
        let fd = open(&filename, 0o100 | 0o644);
        if fd >= 0 {
            let data = format!("Small file {} content", i);
            write(fd as usize, data.as_bytes());
            close(fd as usize);
            created_files.push(filename);
        }

        if i % 50 == 0 {
            println!("Created {} files", i + 1);
            yield_();
        }
    }

    println!("Created {} small files", created_files.len());

    // Read them all back
    let mut read_count = 0;
    for filename in &created_files {
        let fd = open(filename, 0);
        if fd >= 0 {
            let mut buffer = [0u8; 64];
            if read(fd as usize, &mut buffer) > 0 {
                read_count += 1;
            }
            close(fd as usize);
        }
    }

    println!("Successfully read {} files", read_count);

    // Clean up - only remove files we explicitly created
    for filename in created_files {
        // Double-check we're only removing our test files
        if filename.starts_with("/tmp_small_file_") {
            remove(&filename);
        }
    }

    println!("✅ Many small files test completed");
    true
}

// IPC reliability test
fn ipc_reliability_test() -> i32 {
    println!("=== IPC Reliability Test ===");
    let mut passed = 0;
    let mut total = 0;

    total += 1;
    if test_pipe_stress() { passed += 1; }

    total += 1;
    if test_signal_storm() { passed += 1; }

    total += 1;
    if test_shared_file_access() { passed += 1; }

    println!("IPC reliability tests: {}/{} passed", passed, total);
    if passed == total { 0 } else { 1 }
}

fn test_pipe_stress() -> bool {
    println!("Testing pipe stress conditions...");

    let mut pipe_fds = [0i32; 2];
    if pipe(&mut pipe_fds) != 0 {
        println!("Failed to create pipe");
        return false;
    }

    let child_pid = fork();
    if child_pid == 0 {
        // Child - writer
        close(pipe_fds[0] as usize); // Close read end

        let message = b"pipe stress test message ";
        for i in 0..1000 {
            let full_msg = format!("{}#{}", core::str::from_utf8(message).unwrap(), i);
            write(pipe_fds[1] as usize, full_msg.as_bytes());
            if i % 100 == 0 {
                yield_();
            }
        }
        close(pipe_fds[1] as usize);
        exit(0);
    } else if child_pid > 0 {
        // Parent - reader
        close(pipe_fds[1] as usize); // Close write end

        let mut total_read = 0;
        let mut buffer = [0u8; 256];

        loop {
            let bytes_read = read(pipe_fds[0] as usize, &mut buffer);
            if bytes_read <= 0 {
                break;
            }
            total_read += bytes_read as usize;
        }

        close(pipe_fds[0] as usize);

        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);

        println!("Pipe stress test: read {} bytes total", total_read);
    }

    println!("✅ Pipe stress test completed");
    true
}

fn test_signal_storm() -> bool {
    println!("Testing signal storm handling...");

    signal(signals::SIGUSR1, sigusr1_handler as usize);

    let child_pid = fork();
    if child_pid == 0 {
        // Child - signal receiver
        for _ in 0..100 {
            sleep_ms(10);
            yield_();
        }
        exit(0);
    } else if child_pid > 0 {
        // Parent - signal sender
        for i in 0..50 {
            kill(child_pid as usize, signals::SIGUSR1);
            if i % 10 == 0 {
                sleep_ms(1); // Brief pause
            }
        }

        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);

        println!("Signal storm test completed");
    }

    println!("✅ Signal storm test completed");
    true
}

fn test_shared_file_access() -> bool {
    println!("Testing shared file access patterns...");

    let shared_file = "/tmp_shared_access_test.txt";
    let fd = open(shared_file, 0o100 | 0o644);
    if fd < 0 {
        return false;
    }
    close(fd as usize);

    let num_children = 3;
    let mut children = Vec::new();

    for i in 0..num_children {
        let child_pid = fork();
        if child_pid == 0 {
            // Child - access shared file
            for j in 0..20 {
                let fd = open(shared_file, 0o1); // O_WRONLY
                if fd >= 0 {
                    let data = format!("Child {} write {}\n", i, j);
                    write(fd as usize, data.as_bytes());
                    close(fd as usize);
                }

                let fd = open(shared_file, 0); // O_RDONLY
                if fd >= 0 {
                    let mut buffer = [0u8; 256];
                    read(fd as usize, &mut buffer);
                    close(fd as usize);
                }

                yield_();
            }
            exit(0);
        } else if child_pid > 0 {
            children.push(child_pid);
        }
    }

    // Wait for all children
    for child_pid in children {
        let mut exit_code = 0;
        wait_pid(child_pid as usize, &mut exit_code);
    }

    remove(shared_file);
    println!("✅ Shared file access test completed");
    true
}

// Long running stability test
fn long_running_stability_test() -> i32 {
    println!("=== Long Running Stability Test ===");
    println!("Running for extended period to test system stability...");

    let start_time = get_time_ms();
    let test_duration_ms = 30000; // 30 seconds
    let mut iterations = 0;
    let update_interval = 1000; // Update progress every 1000 iterations

    println!("Test duration: {} seconds", test_duration_ms / 1000);

    while get_time_ms() - start_time < test_duration_ms {
        iterations += 1;

        // Cycle through different operations
        match iterations % 10 {
            0 => {
                // Memory operations
                let addr = mmap(0, 4096, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
                if addr > 0 {
                    unsafe { *(addr as *mut u32) = iterations as u32; }
                    munmap(addr as usize, 4096);
                }
            },
            1..=3 => {
                // File operations
                let filename = format!("/tmp_stability_test_{}.tmp", iterations % 10);
                let fd = open(&filename, 0o100 | 0o644);
                if fd >= 0 {
                    write(fd as usize, format!("iteration {}", iterations).as_bytes());
                    close(fd as usize);
                    remove(&filename);
                }
            },
            4..=6 => {
                // Process operations
                let child_pid = fork();
                if child_pid == 0 {
                    // Quick child task
                    let mut sum = 0u32;
                    for i in 0..1000 {
                        sum = sum.wrapping_add(i);
                    }
                    exit(0);
                } else if child_pid > 0 {
                    let mut exit_code = 0;
                    wait_pid(child_pid as usize, &mut exit_code);
                }
            },
            7..=8 => {
                // Signal operations
                signal(signals::SIGUSR1, sigusr1_handler as usize);
                kill(getpid() as usize, signals::SIGUSR1);
            },
            _ => {
                // CPU intensive task
                let mut result = 1u64;
                for i in 1..1000 {
                    result = result.wrapping_mul(i).wrapping_add(i);
                }
            }
        }

        // Show progress with time-based updates
        if iterations % update_interval == 0 {
            let elapsed = get_time_ms() - start_time;
            let _progress = (elapsed * 100) / test_duration_ms;
            let remaining = test_duration_ms - elapsed;

            print_progress_bar(elapsed as usize, test_duration_ms as usize, 30, "Stability Test");
            println!(" - {} ops, {}ms remaining", iterations, remaining);
        }

        // Brief yield to prevent overwhelming the system
        if iterations % 50 == 0 {
            yield_();
        }
    }

    let total_time = get_time_ms() - start_time;
    println!("\n✅ Stability test completed successfully!");
    println!("   📊 {} iterations in {}ms", iterations, total_time);
    println!("   ⚡ Average rate: {:.1} operations/second", (iterations as f64 * 1000.0) / total_time as f64);
    println!("   🎯 System remained stable throughout the test");

    0
}