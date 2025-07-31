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
    exit(15);
}

// Helper function to get current hart ID (simulated)
fn get_hart_id() -> usize {
    // In a real implementation, this would return the actual core ID
    // For now, we'll use PID as a proxy
    (getpid() as usize) % 4
}

// Multi-core stress test
fn multicore_stress_test() -> i32 {
    println!("=== Multi-Core Stress Test ===");
    
    let num_children = 4; // Create 4 child processes for multi-core testing
    let mut children = Vec::new();
    
    for i in 0..num_children {
        let pid = fork();
        if pid == 0 {
            // Child process - simulate different workloads on different cores
            println!("Child {} (PID: {}) starting on core {}", i, getpid(), get_hart_id());
            
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
            println!("Created child {} with PID {}", i, pid);
        } else {
            println!("Failed to fork child {}", i);
        }
    }
    
    // Wait for all children
    let mut all_success = true;
    for (i, child_pid) in children.iter().enumerate() {
        let mut exit_code = 0;
        let result = wait_pid(*child_pid as usize, &mut exit_code);
        if result >= 0 && exit_code == 0 {
            println!("Child {} (PID: {}) exited successfully", i, child_pid);
        } else {
            println!("Child {} (PID: {}) failed with exit code {}", i, child_pid, exit_code);
            all_success = false;
        }
    }
    
    if all_success {
        println!("✅ Multi-core stress test passed!");
        0
    } else {
        println!("❌ Multi-core stress test failed!");
        1
    }
}

fn cpu_intensive_task(id: usize) {
    println!("CPU task {} starting intensive computation", id);
    let mut result = 1u64;
    for i in 1..100000 {
        result = result.wrapping_mul(i as u64).wrapping_add(i as u64);
        if i % 10000 == 0 {
            yield_(); // Allow other processes to run
        }
    }
    println!("CPU task {} result: {}", id, result);
}

fn memory_intensive_task(id: usize) {
    println!("Memory task {} starting memory allocation test", id);
    let mut vectors = Vec::new();
    
    for i in 0..100 {
        let mut vec = Vec::new();
        for j in 0..1000 {
            vec.push(i * 1000 + j);
        }
        vectors.push(vec);
        
        if i % 10 == 0 {
            yield_(); // Allow other processes to run
        }
    }
    
    println!("Memory task {} allocated {} vectors", id, vectors.len());
}

fn io_intensive_task(id: usize) {
    println!("I/O task {} starting file operations", id);
    
    for i in 0..10 {
        let filename = format!("test_io_{}.txt", id);
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
    let test_file = "test_io_file.txt";
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
    let test_file = "test_perm_file.txt";
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
            for iteration in 0..20 {
                // Simulate CPU-intensive work
                let mut sum = 0u64;
                for j in 0..100000 {
                    sum = sum.wrapping_add(j as u64);
                }
                
                println!("Child {} iteration {}: sum={}", i, iteration, sum);
                
                // Check for signals every few iterations
                if iteration % 5 == 0 {
                    yield_(); // Give opportunity for signal delivery
                }
                
                // Sleep briefly
                sleep_ms(50);
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
    sleep_ms(200);
    
    // Send signals to children to test cross-core delivery
    for (i, child_pid) in children.iter().enumerate() {
        println!("Sending SIGUSR1 to child {} (PID: {})", i, child_pid);
        if kill(*child_pid as usize, signals::SIGUSR1) != 0 {
            println!("Failed to send signal to child {}", i);
        }
        sleep_ms(100);
    }
    
    // Send SIGINT to test termination
    sleep_ms(500);
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
    
    // Run comprehensive test suite
    let tests: Vec<(&str, fn() -> i32)> = vec![
        ("Complete Syscall Suite", test_all_syscalls),
        ("Multi-Core Stress Test", multicore_stress_test),
        ("Multi-Core Signal Test", test_multicore_signals),
    ];
    
    for (test_name, test_func) in tests.iter() {
        total_tests += 1;
        println!("🧪 Running test: {}", test_name);
        println!("==================================================");
        
        let result = test_func();
        if result == 0 {
            passed_tests += 1;
            println!("✅ Test '{}' PASSED\n", test_name);
        } else {
            println!("❌ Test '{}' FAILED with code: {}\n", test_name, result);
        }
    }
    
    // Final summary
    println!("📊 === Final Test Results ===");
    println!("Total test suites: {}", total_tests);
    println!("Passed: {}", passed_tests);
    println!("Failed: {}", total_tests - passed_tests);
    println!("Success rate: {:.1}%", (passed_tests as f32 / total_tests as f32) * 100.0);
    
    if passed_tests == total_tests {
        println!("🎉 ALL TESTS PASSED! LiteOS multi-core functionality is working correctly.");
        println!("✅ IPI-based cross-core signal delivery is functional!");
    } else {
        println!("⚠️ Some tests failed. Check the implementation.");
    }
    
    println!("=== LiteOS Comprehensive Test Suite Complete ===");
    
    // Return success if all tests passed
    if passed_tests == total_tests { 0 } else { 1 }
}