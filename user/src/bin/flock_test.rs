#![no_std]
#![no_main]

use user_lib::*;
use user_lib::flock_consts::*;

fn test_flock_basic() {
    println!("=== Test basic flock functionality ===");
    
    // Use existing test file from filesystem
    let test_file = "/hello.txt";
    let fd = open(test_file, 0);
    if fd < 0 {
        println!("Failed to open file for locking test");
        return;
    }
    
    println!("File descriptor: {}", fd);
    
    // Test shared lock
    println!("1. Test shared lock (LOCK_SH)");
    let result = flock(fd as usize, LOCK_SH);
    if result == 0 {
        println!("   ✓ Successfully acquired shared lock");
    } else {
        println!("   ✗ Failed to acquire shared lock: {}", result);
    }
    
    // Test unlock
    println!("2. Test unlock (LOCK_UN)");
    let result = flock(fd as usize, LOCK_UN);
    if result == 0 {
        println!("   ✓ Successfully unlocked");
    } else {
        println!("   ✗ Failed to unlock: {}", result);
    }
    
    // Test exclusive lock
    println!("3. Test exclusive lock (LOCK_EX)");
    let result = flock(fd as usize, LOCK_EX);
    if result == 0 {
        println!("   ✓ Successfully acquired exclusive lock");
    } else {
        println!("   ✗ Failed to acquire exclusive lock: {}", result);
    }
    
    // Test non-blocking mode
    println!("4. Test non-blocking exclusive lock (LOCK_EX | LOCK_NB)");
    let result = flock(fd as usize, LOCK_EX | LOCK_NB);
    if result == 0 {
        println!("   ✓ Successfully acquired non-blocking exclusive lock");
    } else if result == -11 {
        println!("   ✓ Correctly returned EAGAIN (lock is held)");
    } else {
        println!("   ✗ Non-blocking lock test failed: {}", result);
    }
    
    // Cleanup: unlock and close the file
    flock(fd as usize, LOCK_UN);
    close(fd as usize);
    
    println!("Basic flock tests completed");
}

fn test_flock_multiple_processes() {
    println!("=== Test multi-process file locking ===");
    
    let test_file = "/test.txt";
    
    let pid = fork();
    if pid == 0 {
        // Child process
        sleep(100); // Let parent process acquire the lock first
        
        let fd = open(test_file, 0);
        if fd < 0 {
            println!("Child: Failed to open file");
            exit(1);
        }
        
        println!("Child: Attempting to acquire non-blocking exclusive lock");
        let result = flock(fd as usize, LOCK_EX | LOCK_NB);
        if result == -11 {
            println!("Child: ✓ Correctly blocked by parent's lock");
        } else {
            println!("Child: ✗ Should have been blocked but wasn't: {}", result);
        }
        
        close(fd as usize);
        exit(0);
    } else {
        // Parent process
        let fd = open(test_file, 0);
        if fd < 0 {
            println!("Parent: Failed to open file");
            return;
        }
        
        println!("Parent: Acquiring exclusive lock");
        let result = flock(fd as usize, LOCK_EX);
        if result == 0 {
            println!("Parent: ✓ Successfully acquired exclusive lock");
            
            // Wait for child to try to acquire lock
            sleep(200);
            
            println!("Parent: Releasing lock");
            flock(fd as usize, LOCK_UN);
        } else {
            println!("Parent: ✗ Failed to acquire exclusive lock: {}", result);
        }
        
        close(fd as usize);
        
        // Wait for child process to exit
        let mut exit_code = 0;
        wait_pid(pid as usize, &mut exit_code);
    }
    
    println!("Multi-process flock tests completed");
}

fn test_flock_shared_locks() {
    println!("=== Test shared lock compatibility ===");
    
    let test_file = "/hello.txt";
    
    let pid = fork();
    if pid == 0 {
        // Child process
        sleep(100); // Let parent acquire shared lock first
        
        let fd = open(test_file, 0);
        if fd < 0 {
            println!("Child: Failed to open file");
            exit(1);
        }
        
        println!("Child: Attempting to acquire shared lock");
        let result = flock(fd as usize, LOCK_SH);
        if result == 0 {
            println!("Child: ✓ Successfully acquired shared lock (compatible with parent's shared lock)");
            flock(fd as usize, LOCK_UN);
        } else {
            println!("Child: ✗ Failed to acquire shared lock: {}", result);
        }
        
        close(fd as usize);
        exit(0);
    } else {
        // Parent process
        let fd = open(test_file, 0);
        if fd < 0 {
            println!("Parent: Failed to open file");
            return;
        }
        
        println!("Parent: Acquiring shared lock");
        let result = flock(fd as usize, LOCK_SH);
        if result == 0 {
            println!("Parent: ✓ Successfully acquired shared lock");
            
            // Wait for child to try to acquire shared lock
            sleep(200);
            
            println!("Parent: Releasing shared lock");
            flock(fd as usize, LOCK_UN);
        } else {
            println!("Parent: ✗ Failed to acquire shared lock: {}", result);
        }
        
        close(fd as usize);
        
        // Wait for child process to exit
        let mut exit_code = 0;
        wait_pid(pid as usize, &mut exit_code);
    }
    
    println!("Shared lock compatibility tests completed");
}

fn test_flock_error_cases() {
    println!("=== Test error cases ===");

    // Test invalid file descriptor
    println!("1. Test invalid file descriptor");
    let result = flock(999, LOCK_SH);
    if result == -9 {
        println!("   ✓ Correctly returned EBADF");
    } else {
        println!("   ✗ Should have returned EBADF, but got: {}", result);
    }

    // Test invalid operation
    println!("2. Test invalid operation");
    let fd = open("/hello.txt", 0);
    if fd >= 0 {
        let result = flock(fd as usize, 999);
        if result == -22 {
            println!("   ✓ Correctly returned EINVAL");
        } else {
            println!("   ✗ Should have returned EINVAL, but got: {}", result);
        }
        close(fd as usize);
    }

    println!("Error case tests completed");
}

// Simple sleep implementation
fn sleep(ms: usize) {
    for _ in 0..ms * 1000 {
        yield_();
    }
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("File lock (flock) functionality test program");
    println!("================================");

    test_flock_basic();
    test_flock_error_cases();
    test_flock_multiple_processes();
    test_flock_shared_locks();

    println!("All flock tests completed!");
    0
}