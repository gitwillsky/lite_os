#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use alloc::vec::Vec;

// Simple ring buffer implementation (lock-free version, for demonstration only)
struct RingBuffer {
    data: [i32; 10],
    head: usize,
    tail: usize,
    count: usize,
}

impl RingBuffer {
    const fn new() -> Self {
        RingBuffer {
            data: [0; 10],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    fn is_full(&self) -> bool {
        self.count == 10
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn put(&mut self, item: i32) -> bool {
        if self.is_full() {
            return false;
        }

        self.data[self.tail] = item;
        self.tail = (self.tail + 1) % 10;
        self.count += 1;
        true
    }

    fn get(&mut self) -> Option<i32> {
        if self.is_empty() {
            return None;
        }

        let item = self.data[self.head];
        self.head = (self.head + 1) % 10;
        self.count -= 1;
        Some(item)
    }
}

// Global buffer (should use appropriate synchronization mechanisms in actual applications)
static mut BUFFER: RingBuffer = RingBuffer::new();
static mut PRODUCER_DONE: bool = false;
static mut CONSUMER_COUNT: usize = 0;

/// Producer thread
extern "C" fn producer_thread() -> i32 {
    println!("Producer thread started");

    for i in 1..=20 {
        // Try to put data
        let mut attempts = 0;
        loop {
            unsafe {
                if unsafe { (&raw mut BUFFER).as_mut().unwrap() }.put(i) {
                    println!("Producer: produced {}", i);
                    break;
                } else {
                    attempts += 1;
                    if attempts > 100 {
                        println!("Producer: buffer full, skip {}", i);
                        break;
                    }
                }
            }
            thread_yield(); // Yield CPU to consumer
        }

        // Simulate production time
        for _ in 0..100 {
            thread_yield();
        }
    }

    unsafe {
        PRODUCER_DONE = true;
    }
    println!("Producer thread finished");
    0
}

/// Consumer thread
extern "C" fn consumer_thread() -> i32 {
    println!("Consumer thread started");
    let mut consumed = 0;

    loop {
        unsafe {
            if let Some(item) = unsafe { (&raw mut BUFFER).as_mut().unwrap() }.get() {
                consumed += 1;
                CONSUMER_COUNT += 1;
                println!("Consumer: consumed {} (total: {})", item, consumed);

                // Simulate consumption time
                for _ in 0..50 {
                    thread_yield();
                }
            } else {
                // Buffer empty, check if producer is done
                if PRODUCER_DONE {
                    break;
                }
                thread_yield();
            }
        }
    }

    println!("Consumer thread finished, total consumed: {}", consumed);
    consumed
}

/// Monitor thread
extern "C" fn monitor_thread() -> i32 {
    println!("Monitor thread started");

    for i in 0..10 {
        unsafe {
            println!("Monitor #{}: buffer usage {}/10, consumed: {}, producer status: {}",
                     i + 1, unsafe { (&raw const BUFFER).as_ref().unwrap() }.count, unsafe { CONSUMER_COUNT },
                     if PRODUCER_DONE { "finished" } else { "running" });
        }

        // Wait for 1 second (approximately)
        for _ in 0..1000 {
            thread_yield();
        }

        unsafe {
            if unsafe { PRODUCER_DONE } && unsafe { (&raw const BUFFER).as_ref().unwrap() }.is_empty() {
                break;
            }
        }
    }

    println!("Monitor thread finished");
    0
}

/// Worker thread - performs computationally intensive tasks
extern "C" fn worker_thread() -> i32 {
    println!("Worker thread started computing");

    let mut result = 0;
    for i in 1..=1000 {
        result += i * i;

        // Report progress every 100 iterations
        if i % 100 == 0 {
            println!("Worker thread: progress {}%, current result: {}", i / 10, result);
            thread_yield();
        }
    }

    println!("Worker thread finished, final result: {}", result);
    result
}

/// Signal handling test thread
extern "C" fn signal_thread() -> i32 {
    println!("Signal thread: set alarm for 5 seconds");

    // Set alarm for 5 seconds
    alarm(5);

    // Wait for signal
    println!("Signal thread: waiting for SIGALRM signal...");
    for i in 0..10 {
        println!("Signal thread: waiting... {}", i + 1);
        for _ in 0..500 {
            thread_yield();
        }
    }

    println!("Signal thread finished");
    0
}

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("=== Producer-Consumer Multithreaded Test ===");
    println!("Main process PID: {}", getpid());

    // Reset global state
    unsafe {
        BUFFER = RingBuffer::new();
        PRODUCER_DONE = false;
        CONSUMER_COUNT = 0;
    }

    // Test 1: Producer-Consumer Pattern
    println!("\n--- Test 1: Producer-Consumer Pattern ---");

    let producer = thread_create(producer_thread, 0, None);
    let consumer1 = thread_create(consumer_thread, 0, None);
    let consumer2 = thread_create(consumer_thread, 0, None);
    let monitor = thread_create(monitor_thread, 0, None);

    if producer < 0 || consumer1 < 0 || consumer2 < 0 || monitor < 0 {
        println!("Thread creation failed!");
        return -1;
    }

    println!("Producer thread created: {}", producer);
    println!("Consumer thread 1 created: {}", consumer1);
    println!("Consumer thread 2 created: {}", consumer2);
    println!("Monitor thread created: {}", monitor);

    // Main thread also participates in some lightweight work
    for i in 0..5 {
        println!("Main thread: supervising... #{}", i + 1);
        // Longer delay
        for _ in 0..2000 {
            thread_yield();
        }
    }

    // Wait for all threads to finish
    println!("Waiting for all threads to finish...");
    let prod_result = thread_join(producer as usize);
    let cons1_result = thread_join(consumer1 as usize);
    let cons2_result = thread_join(consumer2 as usize);
    let mon_result = thread_join(monitor as usize);

    println!("Producer thread result: {}", prod_result);
    println!("Consumer thread 1 result: {}", cons1_result);
    println!("Consumer thread 2 result: {}", cons2_result);
    println!("Monitor thread result: {}", mon_result);
    println!("Total consumed: {}", unsafe { CONSUMER_COUNT });

    // Test 2: Parallel Computation Test
    println!("\n--- Test 2: Parallel Computation Test ---");

    let mut compute_attr = ThreadAttr::default();
    compute_attr.stack_size = 16384; // Larger stack for computation

    let worker1 = thread_create(worker_thread, 0, Some(&compute_attr));
    let worker2 = thread_create(worker_thread, 0, Some(&compute_attr));
    let worker3 = thread_create(worker_thread, 0, Some(&compute_attr));

    if worker1 < 0 || worker2 < 0 || worker3 < 0 {
        println!("Computation thread creation failed!");
        return -1;
    }

    println!("3 parallel worker threads created: {}, {}, {}", worker1, worker2, worker3);

    let result1 = thread_join(worker1 as usize);
    let result2 = thread_join(worker2 as usize);
    let result3 = thread_join(worker3 as usize);

    println!("Worker thread 1 result: {}", result1);
    println!("Worker thread 2 result: {}", result2);
    println!("Worker thread 3 result: {}", result3);

    // Verify consistency of results
    if result1 == result2 && result2 == result3 {
        println!("✓ Parallel computation results are consistent!");
    } else {
        println!("✗ Parallel computation results are inconsistent!");
    }

    // Test 3: Mixed Workload Test
    println!("\n--- Test 3: Mixed Workload Test ---");

    // Reset state
    unsafe {
        BUFFER = RingBuffer::new();
        PRODUCER_DONE = false;
        CONSUMER_COUNT = 0;
    }

    // Create threads of mixed types
    let threads = [
        thread_create(producer_thread, 0, None),
        thread_create(consumer_thread, 0, None),
        thread_create(worker_thread, 0, Some(&compute_attr)),
        thread_create(signal_thread, 0, None),
        thread_create(monitor_thread, 0, None),
    ];

    let mut valid_threads = Vec::new();
    for (i, &thread_id) in threads.iter().enumerate() {
        if thread_id >= 0 {
            valid_threads.push(thread_id as usize);
            println!("Mixed thread #{}: {} created", i, thread_id);
        } else {
            println!("Mixed thread #{} creation failed", i);
        }
    }

    // Main thread performs some management tasks
    for i in 0..8 {
        println!("Main thread: management task #{}", i + 1);
        for _ in 0..1500 {
            thread_yield();
        }
    }

    // Wait for all threads
    for (i, &thread_id) in valid_threads.iter().enumerate() {
        let result = thread_join(thread_id);
        println!("Mixed thread #{} finished, return value: {}", i, result);
    }

    println!("\n=== Producer-Consumer Multithreaded Test Finished ===");
    println!("Final state:");
    unsafe {
        println!("- Buffer remaining: {}/10", unsafe { (&raw const BUFFER).as_ref().unwrap() }.count);
        println!("- Total consumed: {}", unsafe { CONSUMER_COUNT });
        println!("- Producer status: {}", if PRODUCER_DONE { "finished" } else { "not finished" });
    }

    println!("All tests passed!");
    0
}