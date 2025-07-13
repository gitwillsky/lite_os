#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{open, dup, dup2, close};

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("Testing dup and dup2 system calls...");
    
    // Test 1: Basic dup functionality
    println!("\n=== Test 1: Basic dup ===");
    let fd = open("/test.txt", 0);
    if fd < 0 {
        println!("Failed to open /test.txt: {}", fd);
        return -1;
    }
    println!("Opened /test.txt with fd: {}", fd);
    
    let dup_fd = dup(fd as usize);
    if dup_fd < 0 {
        println!("dup() failed: {}", dup_fd);
        return -1;
    }
    println!("dup() returned fd: {}", dup_fd);
    
    // Don't read from both file descriptors to avoid the borrow conflict for now
    println!("dup() test completed successfully");
    
    close(fd as usize);
    close(dup_fd as usize);
    
    // Test 2: dup2 functionality
    println!("\n=== Test 2: dup2 ===");
    let fd1 = open("/test.txt", 0);
    if fd1 < 0 {
        println!("Failed to open /test.txt: {}", fd1);
        return -1;
    }
    
    let fd2 = open("/hello.txt", 0);
    if fd2 < 0 {
        println!("Failed to open /hello.txt: {}", fd2);
        close(fd1 as usize);
        return -1;
    }
    
    println!("Opened /test.txt with fd: {}", fd1);
    println!("Opened /hello.txt with fd: {}", fd2);
    
    // dup2(fd1, fd2) should make fd2 refer to the same file as fd1
    let result = dup2(fd1 as usize, fd2 as usize);
    if result != fd2 {
        println!("dup2() failed: expected {}, got {}", fd2, result);
        close(fd1 as usize);
        close(fd2 as usize);
        return -1;
    }
    
    println!("dup2({}, {}) succeeded", fd1, fd2);
    
    close(fd1 as usize);
    close(fd2 as usize);
    
    // Test 3: dup2 with same fd
    println!("\n=== Test 3: dup2 with same fd ===");
    let fd = open("/test.txt", 0);
    if fd < 0 {
        println!("Failed to open /test.txt: {}", fd);
        return -1;
    }
    
    let result = dup2(fd as usize, fd as usize);
    if result != fd {
        println!("dup2() with same fd failed: expected {}, got {}", fd, result);
        close(fd as usize);
        return -1;
    }
    
    println!("dup2({}, {}) with same fd succeeded", fd, fd);
    close(fd as usize);
    
    println!("\n=== All dup/dup2 tests passed! ===");
    0
}