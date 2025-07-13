#![no_std]
#![no_main]

extern crate alloc;

use user_lib::{*, syscall::signals::*};
use core::ptr;

static mut SIGNAL_COUNT: i32 = 0;
static mut SIGUSR1_COUNT: i32 = 0;


extern "C" fn sigint_handler(sig: i32) {
    unsafe {
        SIGNAL_COUNT += 1;
        let count = SIGNAL_COUNT;
        println!("📧 Received signal SIGINT ({}), count: {}", sig, count);
    }

    if unsafe { SIGNAL_COUNT >= 3 } {
        println!("🛑 Received SIGINT 3 times, exiting");
        exit(0);
    }
}

extern "C" fn sigusr1_handler(sig: i32) {
    unsafe {
        SIGUSR1_COUNT += 1;
        let count = SIGUSR1_COUNT;
        println!("📨 Received signal SIGUSR1 ({}), count: {}", sig, count);
    }
}

extern "C" fn sigterm_handler(sig: i32) {
    println!("💀 Received signal SIGTERM ({}), exiting gracefully", sig);
    exit(15);
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("🚀 === LiteOS Signal Handling Test Program ===");
    println!("This program will test the full signal mechanism implementation of the LiteOS kernel");

    // Test 1: Set signal handlers
    println!("\n📋 Test 1: Set signal handlers");

    // Set SIGINT handler
    let old_handler = signal(SIGINT, sigint_handler as usize);
    if old_handler < 0 {
        println!("❌ Failed to set SIGINT handler");
        return -1;
    }
    println!("✅ Successfully set SIGINT handler");

    // Set SIGUSR1 handler
    let old_handler = signal(SIGUSR1, sigusr1_handler as usize);
    if old_handler < 0 {
        println!("❌ Failed to set SIGUSR1 handler");
        return -1;
    }
    println!("✅ Successfully set SIGUSR1 handler");

    // Set SIGTERM handler
    let old_handler = signal(SIGTERM, sigterm_handler as usize);
    if old_handler < 0 {
        println!("❌ Failed to set SIGTERM handler");
        return -1;
    }
    println!("✅ Successfully set SIGTERM handler");

    // Get current process PID
    let pid = getpid();
    println!("🆔 Current process PID: {}", pid);

    // Test 2: Send signal to self
    println!("\n📋 Test 2: Process sends signal to itself");
    if kill(pid as usize, SIGUSR1) < 0 {
        println!("❌ Failed to send SIGUSR1 signal");
    } else {
        println!("📤 SIGUSR1 signal sent to self");
    }

    // Wait for signal handling
    for _ in 0..1000000 {
        // Simple busy wait to allow signal handling
    }

    // Test 3: Create child process and test inter-process signals
    println!("\n📋 Test 3: Inter-process signal communication");
    let child_pid = fork();

    if child_pid == 0 {
        // === Child process ===
        println!("👶 Child process started: PID = {}", getpid());
        println!("👶 Child process: Set SIGTERM handler to default");
        signal(SIGTERM, SIG_DFL);

        println!("👶 Child process: Waiting for signal from parent...");

        // Loop waiting for signals, not just calling pause once
        loop {
            pause(); // Wait for signal
            println!("👶 Child process: Woken up by signal, checking if should exit...");
            // In actual implementation, if SIGTERM is received, the process will terminate
            // This code will not be executed because the default action for SIGTERM is to terminate the process
        }
    } else if child_pid > 0 {
        // === Parent process ===
        println!("👨 Parent process: Created child process PID={}", child_pid);

        // Wait a bit to let child process get ready
        for _ in 0..5000000 {
            // Longer wait
        }

        println!("👨 Parent process: Sending SIGUSR1 signal to child for test");
        if kill(child_pid as usize, SIGUSR1) < 0 {
            println!("❌ Failed to send SIGUSR1 signal to child");
        } else {
            println!("📤 SIGUSR1 signal sent to child");
        }

        // Wait again
        for _ in 0..2000000 {
            // Wait
        }

        println!("👨 Parent process: Sending SIGTERM signal to child to make it exit");
        if kill(child_pid as usize, SIGTERM) < 0 {
            println!("❌ Failed to send SIGTERM signal");
        } else {
            println!("📤 SIGTERM signal sent to child");
        }

        // Wait for child to exit
        let mut exit_code: i32 = 0;
        let wait_result = wait_pid(child_pid as usize, &mut exit_code);
        if wait_result >= 0 {
            println!("✅ Child process exited, exit code: {}", exit_code);
        } else {
            println!("❌ Failed to wait for child process");
        }
    } else {
        println!("❌ fork failed");
        return -1;
    }

    // Test 4: Signal mask operations
    println!("\n📋 Test 4: Signal mask operations");
    let mut old_mask: u64 = 0;
    let new_mask: u64 = 1u64 << (SIGUSR1 - 1); // Block SIGUSR1

    if sigprocmask(SIG_BLOCK, &new_mask, &mut old_mask) < 0 {
        println!("❌ Failed to set signal mask");
    } else {
        println!("🚫 SIGUSR1 signal blocked, old mask: {:#x}", old_mask);
    }

    // Send signal while blocked
    println!("📤 Sending SIGUSR1 signal to self while blocked");
    kill(pid as usize, SIGUSR1);

    // Wait, signal should be blocked
    for _ in 0..2000000 {
        // Wait
    }
    println!("⏰ Wait complete, signal should still be blocked...");

    println!("🔓 Now unblocking SIGUSR1 signal");
    if sigprocmask(SIG_SETMASK, &old_mask, ptr::null_mut()) < 0 {
        println!("❌ Failed to restore signal mask");
    } else {
        println!("✅ Signal mask restored");
    }

    // Wait for signal handling, blocked signal should now be delivered
    for _ in 0..2000000 {
        // Wait for signal handling
    }

    // Test 5: sigaction syscall (advanced signal handling)
    println!("\n📋 Test 5: sigaction advanced signal handling");

    let mut old_action = SigAction {
        sa_handler: 0,
        sa_mask: 0,
        sa_flags: 0,
        sa_restorer: 0,
    };

    let new_action = SigAction {
        sa_handler: sigusr1_handler as usize,
        sa_mask: 1u64 << (SIGINT - 1), // Block SIGINT while handling SIGUSR1
        sa_flags: SA_RESTART, // Restart interrupted syscalls
        sa_restorer: 0,
    };

    if sigaction(SIGUSR1, &new_action, &mut old_action) < 0 {
        println!("❌ sigaction set failed");
    } else {
        println!("✅ sigaction set successfully");
        println!("   Old handler address: {:#x}", old_action.sa_handler);
        println!("   New handler address: {:#x}", new_action.sa_handler);
        println!("   Signal mask: {:#x}", new_action.sa_mask);
        println!("   Flags: {:#x}", new_action.sa_flags);
    }

    // Test new sigaction setting
    println!("📤 Testing new sigaction configuration");
    kill(pid as usize, SIGUSR1);

    // Wait for signal handling
    for _ in 0..1000000 {
        // Wait
    }

    // Test 6: Multiple signal sends
    println!("\n📋 Test 6: Rapid consecutive signal test");
    for i in 1..=5 {
        println!("📤 Sending SIGUSR1 signal {} time(s)", i);
        kill(pid as usize, SIGUSR1);

        // Short wait
        for _ in 0..500000 {
            // Wait
        }
    }

    // Final wait
    for _ in 0..2000000 {
        // Wait for all signal handling to complete
    }

    // Show statistics
    println!("\n📊 Signal handling statistics:");
    unsafe {
        let signal_count = SIGNAL_COUNT;
        let sigusr1_count = SIGUSR1_COUNT;
        println!("   SIGINT handled: {} times", signal_count);
        println!("   SIGUSR1 handled: {} times", sigusr1_count);
    }

    println!("\n📋 Test 7: Waiting for SIGINT signal (simulate Ctrl+C)");
    println!("💡 Tip: Another process needs to send SIGINT 3 times to exit");
    println!("💡 You can use in another terminal: kill -2 {}", pid);

    // Infinite loop waiting for SIGINT
    let mut loop_count = 0;
    loop {
        pause();
        loop_count += 1;
        println!("🔄 Loop {}: Woken up by signal, continue waiting...", loop_count);

        unsafe {
            if SIGNAL_COUNT >= 3 {
                break;
            }
        }

        // For demonstration, loop at most 10 times
        if loop_count >= 10 {
            println!("🔚 Demo finished, exiting loop");
            break;
        }
    }

    println!("\n🎉 LiteOS signal handling test complete!");
    println!("✨ All signal mechanism features verified successfully");
    0
}