#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use alloc::vec::Vec;
use alloc::string::String;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("=== Kernel-backed Heap Test ===");
    
    // 测试基本的 Vec 分配
    println!("Testing Vec allocation...");
    let mut vec = Vec::new();
    
    for i in 0..10 {
        vec.push(i * 2);
    }
    
    println!("Vec contents: {:?}", vec);
    
    // 测试 String 分配
    println!("Testing String allocation...");
    let mut s = String::new();
    s.push_str("Hello, ");
    s.push_str("World!");
    
    println!("String: {}", s);
    
    // 测试大量小分配
    println!("Testing many small allocations...");
    let mut vecs = Vec::new();
    for i in 0..100 {
        let mut v = Vec::new();
        v.push(i);
        vecs.push(v);
    }
    
    println!("Created {} small vectors", vecs.len());
    
    // 测试大分配
    println!("Testing large allocation...");
    let large_vec: Vec<u32> = (0..1000).collect();
    println!("Large vec size: {}", large_vec.len());
    
    // 释放内存（自动进行）
    drop(vec);
    drop(s);
    drop(vecs);
    drop(large_vec);
    
    println!("✓ All heap tests passed!");
    println!("=== Test Complete ===");
    0
}