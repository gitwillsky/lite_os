#![no_std]
#![no_main]

use user_lib::*;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("=== Memory Management Test ===");
    
    // 测试 brk 系统调用
    println!("Testing brk system call...");
    
    // 获取当前堆顶
    let initial_brk = brk(0);
    println!("Initial brk: {:#x}", initial_brk);
    
    // 扩展堆
    let new_size = 4096; // 4KB
    let new_brk = brk(initial_brk as usize + new_size);
    if new_brk > 0 {
        println!("Extended heap to: {:#x}", new_brk);
        
        // 测试写入内存
        unsafe {
            let ptr = initial_brk as *mut u8;
            *ptr = 0x42;
            let value = *ptr;
            println!("Wrote 0x42 to heap, read back: 0x{:x}", value);
            
            if value == 0x42 {
                println!("✓ Heap write/read test passed");
            } else {
                println!("✗ Heap write/read test failed");
            }
        }
    } else {
        println!("✗ Failed to extend heap");
    }
    
    // 测试 sbrk 系统调用
    println!("\nTesting sbrk system call...");
    
    let current_brk = sbrk(0);
    println!("Current brk: {:#x}", current_brk);
    
    // 增加 4KB
    let old_brk = sbrk(4096);
    if old_brk > 0 {
        println!("sbrk(4096) returned old brk: {:#x}", old_brk);
        
        let new_brk = sbrk(0);
        println!("New brk: {:#x}", new_brk);
        
        if new_brk as usize == old_brk as usize + 4096 {
            println!("✓ sbrk test passed");
        } else {
            println!("✗ sbrk test failed");
        }
    } else {
        println!("✗ sbrk failed");
    }
    
    // 测试 mmap 系统调用
    println!("\nTesting mmap system call...");
    
    // 映射 4KB 内存 (读写权限)
    let addr = mmap(0, 4096, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE);
    if addr > 0 {
        println!("mmap allocated memory at: {:#x}", addr);
        
        // 测试写入映射的内存
        unsafe {
            let ptr = addr as *mut u32;
            *ptr = 0x12345678;
            let value = *ptr;
            println!("Wrote 0x12345678 to mapped memory, read back: 0x{:x}", value);
            
            if value == 0x12345678 {
                println!("✓ mmap write/read test passed");
            } else {
                println!("✗ mmap write/read test failed");
            }
        }
        
        // 测试 munmap
        let result = munmap(addr as usize, 4096);
        if result == 0 {
            println!("✓ munmap succeeded");
        } else {
            println!("✗ munmap failed");
        }
    } else {
        println!("✗ mmap failed");
    }
    
    println!("\n=== Memory Management Test Complete ===");
    0
}