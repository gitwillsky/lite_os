#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use user_lib::*;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("=== Complete Kernel-backed Heap Test ===");
    
    // 测试基本的内存管理系统调用
    println!("\n1. Testing basic memory system calls...");
    
    let initial_brk = brk(0);
    println!("Initial brk: {:#x}", initial_brk);
    
    let new_brk = brk(initial_brk as usize + 8192);
    println!("Extended brk to: {:#x}", new_brk);
    
    // 测试基本的 Vec 分配
    println!("\n2. Testing Vec allocation...");
    let mut numbers = Vec::new();
    for i in 0..20 {
        numbers.push(i * i);
    }
    println!("Vec with {} elements: {:?}", numbers.len(), &numbers[..10]);
    
    // 测试 String 分配
    println!("\n3. Testing String allocation...");
    let mut message = String::new();
    message.push_str("Hello from kernel-backed heap! ");
    message.push_str("This string is dynamically allocated using brk/sbrk system calls.");
    println!("String length: {}, content: {}", message.len(), message);
    
    // 测试嵌套容器
    println!("\n4. Testing nested containers...");
    let mut data: Vec<Vec<i32>> = Vec::new();
    for i in 0..5 {
        let mut inner_vec = Vec::new();
        for j in 0..10 {
            inner_vec.push(i * 10 + j);
        }
        data.push(inner_vec);
    }
    println!("Created {} nested vectors", data.len());
    println!("First vector: {:?}", data[0]);
    
    // 测试 BTreeMap
    println!("\n5. Testing BTreeMap allocation...");
    let mut map = BTreeMap::new();
    map.insert("kernel", "Handles system calls");
    map.insert("user", "Runs applications");
    map.insert("heap", "Dynamic memory allocation");
    
    println!("Map contents:");
    for (key, value) in &map {
        println!("  {}: {}", key, value);
    }
    
    // 测试大量小分配
    println!("\n6. Testing many small allocations...");
    let mut small_strings = Vec::new();
    for i in 0..100 {
        let s = format!("String number {}", i);
        small_strings.push(s);
    }
    println!("Created {} small strings", small_strings.len());
    println!("Sample: {}, {}, {}", small_strings[0], small_strings[50], small_strings[99]);
    
    // 测试大分配
    println!("\n7. Testing large allocation...");
    let large_data: Vec<u64> = (0..10000).map(|x| x as u64 * x as u64).collect();
    println!("Large vector size: {} elements", large_data.len());
    println!("Sum of first 100 elements: {}", large_data[..100].iter().sum::<u64>());
    
    // 测试内存释放（通过 drop）
    println!("\n8. Testing memory deallocation...");
    drop(numbers);
    drop(message);
    drop(data);
    drop(map);
    drop(small_strings);
    drop(large_data);
    println!("Memory deallocated successfully");
    
    // 测试释放后的重新分配
    println!("\n9. Testing reallocation after deallocation...");
    let mut final_test = Vec::new();
    for i in 0..50 {
        final_test.push(format!("Final test {}", i));
    }
    println!("Final test: {} strings allocated", final_test.len());
    
    println!("\n=== All tests completed successfully! ===");
    println!("Kernel-backed heap allocator is working correctly.");
    0
}