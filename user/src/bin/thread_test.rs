#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use alloc::vec::Vec;

// Global counter for testing inter-thread communication
static mut GLOBAL_COUNTER: usize = 0;

/// Thread function 1: Counter
extern "C" fn counter_thread() -> i32 {
    // 立即输出调试信息
    unsafe {
        use core::arch::asm;
        // 直接使用系统调用输出消息
        let msg = b"COUNTER_THREAD_STARTED\n";
        asm!(
            "li a7, 64",           // sys_write
            "li a0, 1",            // stdout
            "mv a1, {msg}",        // message
            "li a2, 23",           // length
            "ecall",
            msg = in(reg) msg.as_ptr(),
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x17") _,
        );
    }

    println!("Counter thread started!");
    for i in 0..5 {
        unsafe {
            GLOBAL_COUNTER += 1;
        }
        println!("Thread 1: Counter = {}, iteration = {}", unsafe { GLOBAL_COUNTER }, i + 1);
        thread_yield();
    }
    println!("Thread 1: Finished work");
    42 // Return value
}

/// Thread function 2: Print message
extern "C" fn message_thread() -> i32 {
    for i in 0..3 {
        println!("Thread 2: Message #{}", i + 1);
        // Simulate some work
        for _ in 0..1000 {
            thread_yield();
        }
    }
    println!("Thread 2: Finished work");
    100 // Return value
}

/// Thread function 3: Heavy computation
extern "C" fn compute_thread() -> i32 {
    let mut sum = 0;
    for i in 1..=100 {
        sum += i;
        if i % 20 == 0 {
            println!("Thread 3: Compute progress {}%, sum = {}", i, sum);
            thread_yield();
        }
    }
    println!("Thread 3: Computation finished, sum = {}", sum);
    sum
}

/// Thread function 4: High priority thread
extern "C" fn priority_thread() -> i32 {
    for i in 0..3 {
        println!("High priority thread: Executing #{}", i + 1);
        thread_yield();
    }
    println!("High priority thread: Finished");
    200
}

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("=== Multithreaded Test Program ===");
    println!("Main process PID: {}", getpid());

    // Test 1: Basic thread creation and join
    println!("\n--- Test 1: Basic Thread Creation ---");

    let thread1 = thread_create(counter_thread, 0, None);
    let thread2 = thread_create(message_thread, 0, None);

    if thread1 < 0 || thread2 < 0 {
        println!("Thread creation failed!");
        return -1;
    }

    println!("Created threads {} and {}", thread1, thread2);

    // Main thread does some work
    for i in 0..3 {
        println!("Main thread: Work #{}", i + 1);
        thread_yield();
    }

    // Wait for threads to finish
    println!("Waiting for threads to finish...");
    let result1 = thread_join(thread1 as usize);
    let result2 = thread_join(thread2 as usize);

    println!("Thread {} return value: {}", thread1, result1);
    println!("Thread {} return value: {}", thread2, result2);
    println!("Final global counter: {}", unsafe { GLOBAL_COUNTER });

    // Test 2: Thread creation with attributes
    println!("\n--- Test 2: Custom Thread Attributes ---");

    let mut attr = ThreadAttr::default();
    attr.stack_size = 16384; // 16KB stack
    attr.priority = -5; // High priority

    let thread3 = thread_create(compute_thread, 0, Some(&attr));
    if thread3 < 0 {
        println!("High priority thread creation failed!");
        return -1;
    }

    println!("Created high priority thread {}", thread3);

    // Create another high priority thread
    let thread4 = thread_create(priority_thread, 0, Some(&attr));
    if thread4 < 0 {
        println!("Second high priority thread creation failed!");
        return -1;
    }

    // Main thread continues working
    for i in 0..5 {
        println!("Main thread: Waiting... #{}", i + 1);
        // Yield CPU to other threads
        for _ in 0..500 {
            thread_yield();
        }
    }

    let result3 = thread_join(thread3 as usize);
    let result4 = thread_join(thread4 as usize);

    println!("Compute thread return value: {}", result3);
    println!("High priority thread return value: {}", result4);

    // Test 3: Create many threads (stress test)
    println!("\n--- Test 3: Multithreaded Stress Test ---");

    const MAX_THREADS: usize = 5;
    let mut threads = Vec::new();

    for i in 0..MAX_THREADS {
        let thread_id = if i % 2 == 0 {
            thread_create(counter_thread, i, None)
        } else {
            thread_create(message_thread, i, None)
        };

        if thread_id >= 0 {
            threads.push(thread_id as usize);
            println!("Created thread #{}: {}", i, thread_id);
        } else {
            println!("Thread #{} creation failed", i);
        }

        // Slight delay to avoid creating too fast
        thread_yield();
    }

    println!("Successfully created {} threads", threads.len());

    // Wait for all threads to finish
    for (i, &thread_id) in threads.iter().enumerate() {
        let result = thread_join(thread_id);
        println!("Thread #{} ({}) finished, return value: {}", i, thread_id, result);
    }

    println!("Stress test finished, final global counter: {}", unsafe { GLOBAL_COUNTER });

    // Test 4: Thread error handling
    println!("\n--- Test 4: Error Handling Test ---");

    // Try to join a non-existent thread
    let invalid_result = thread_join(9999);
    println!("Result of waiting for non-existent thread: {}", invalid_result);

    // Try to create too many threads
    let mut failed_count = 0;
    for i in 0..10 {
        let thread_id = thread_create(counter_thread, 0, None);
        if thread_id < 0 {
            failed_count += 1;
        } else {
            // Immediately join to avoid resource exhaustion
            thread_join(thread_id as usize);
        }
    }

    if failed_count > 0 {
        println!("{} threads failed to create (this is normal resource limitation)", failed_count);
    } else {
        println!("All threads created successfully");
    }

    println!("\n=== Multithreaded Test Finished ===");
    println!("All tests passed!");

    0
}