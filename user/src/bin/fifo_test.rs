#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use alloc::string::String;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("Named Pipe (FIFO) functionality test program");
    println!("===========================================");

    // Test 1: Create a named pipe (FIFO)
    println!("\n=== Test 1: Create FIFO ===");
    let fifo_path = "/tmp/test_fifo";
    
    println!("Creating FIFO at: {}", fifo_path);
    let result = mkfifo(fifo_path, 0o644);
    if result == 0 {
        println!("✓ FIFO created successfully");
    } else {
        println!("✗ Failed to create FIFO: {}", result);
        return 1;
    }

    // Test 2: Try to create the same FIFO again (should fail)
    println!("\n=== Test 2: Duplicate FIFO creation ===");
    let result = mkfifo(fifo_path, 0o644);
    if result == -17 {  // EEXIST
        println!("✓ Correctly failed to create duplicate FIFO (EEXIST)");
    } else {
        println!("✗ Should have failed with EEXIST, but got: {}", result);
    }

    // Test 3: Basic FIFO communication using fork
    println!("\n=== Test 3: Basic FIFO communication ===");
    
    let pid = fork();
    if pid == 0 {
        // Child process - writer
        println!("Child: Opening FIFO for writing...");
        let fd = open(fifo_path, 1); // O_WRONLY
        if fd < 0 {
            println!("Child: Failed to open FIFO for writing: {}", fd);
            exit(1);
        }
        
        let message = "Hello from child process!";
        println!("Child: Writing message: {}", message);
        let bytes_written = write(fd as usize, message.as_bytes());
        if bytes_written > 0 {
            println!("Child: ✓ Wrote {} bytes", bytes_written);
        } else {
            println!("Child: ✗ Failed to write: {}", bytes_written);
        }
        
        close(fd as usize);
        println!("Child: Closed FIFO writer");
        exit(0);
    } else {
        // Parent process - reader
        println!("Parent: Opening FIFO for reading...");
        let fd = open(fifo_path, 0); // O_RDONLY
        if fd < 0 {
            println!("Parent: Failed to open FIFO for reading: {}", fd);
            return 1;
        }
        
        let mut buffer = [0u8; 100];
        println!("Parent: Reading from FIFO...");
        let bytes_read = read(fd as usize, &mut buffer);
        if bytes_read > 0 {
            // Convert bytes to string manually since we're in no_std
            let mut message = String::new();
            for i in 0..bytes_read as usize {
                message.push(buffer[i] as char);
            }
            println!("Parent: ✓ Read {} bytes: {}", bytes_read, message);
        } else {
            println!("Parent: ✗ Failed to read: {}", bytes_read);
        }
        
        close(fd as usize);
        println!("Parent: Closed FIFO reader");
        
        // Wait for child process
        let mut exit_code = 0;
        wait_pid(pid as usize, &mut exit_code);
        println!("Parent: Child process exited with code: {}", exit_code);
    }

    // Test 4: Cleanup - try to remove the FIFO
    println!("\n=== Test 4: FIFO cleanup ===");
    let result = remove(fifo_path);
    if result == 0 {
        println!("✓ FIFO removed successfully");
    } else {
        println!("Note: FIFO removal result: {} (may not be implemented yet)", result);
    }

    println!("\nAll FIFO tests completed!");
    0
}