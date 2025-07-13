#![no_std]
#![no_main]

use user_lib::*;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("Simple FIFO test");
    println!("================");

    // Test creating a FIFO
    let fifo_path = "/test_pipe";
    println!("Creating FIFO: {}", fifo_path);
    
    let result = mkfifo(fifo_path, 0o644);
    if result == 0 {
        println!("✓ FIFO created successfully");
    } else {
        println!("✗ Failed to create FIFO: {}", result);
        return 1;
    }

    // Test opening the FIFO
    println!("Opening FIFO for reading...");
    let fd = open(fifo_path, 0);
    if fd >= 0 {
        println!("✓ FIFO opened successfully, fd: {}", fd);
        close(fd as usize);
        println!("✓ FIFO closed");
    } else {
        println!("✗ Failed to open FIFO: {}", fd);
    }

    println!("Simple FIFO test completed");
    0
}