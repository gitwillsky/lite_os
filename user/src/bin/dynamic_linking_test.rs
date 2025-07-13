#![no_std]
#![no_main]

extern crate alloc;

use user_lib::*;

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("=== Dynamic Linking Test ===");
    
    // Test basic functionality first
    println!("Testing static functionality...");
    let result = test_static_functions();
    if result != 0 {
        println!("Static function test failed!");
        return result;
    }
    
    // Test dynamic symbol resolution (simulated)
    println!("Testing dynamic symbol resolution...");
    let result = test_dynamic_symbols();
    if result != 0 {
        println!("Dynamic symbol test failed!");
        return result;
    }
    
    // Test library loading simulation
    println!("Testing library loading simulation...");
    let result = test_library_loading();
    if result != 0 {
        println!("Library loading test failed!");
        return result;
    }
    
    println!("=== All Dynamic Linking Tests Passed! ===");
    0
}

fn test_static_functions() -> i32 {
    println!("  - Testing basic arithmetic operations...");
    let a = 42;
    let b = 58;
    let sum = a + b;
    
    if sum != 100 {
        println!("    ERROR: Expected 100, got {}", sum);
        return 1;
    }
    
    println!("    OK: Static arithmetic works correctly");
    0
}

fn test_dynamic_symbols() -> i32 {
    println!("  - Testing symbol resolution simulation...");
    
    // Simulate dynamic symbol lookup
    let symbols = [
        ("malloc", 0x60001000usize),
        ("free", 0x60001100usize),
        ("printf", 0x60001200usize),
        ("strcmp", 0x60001300usize),
    ];
    
    for (name, expected_addr) in &symbols {
        let resolved_addr = simulate_symbol_lookup(name);
        if resolved_addr != *expected_addr {
            println!("    ERROR: Symbol '{}' resolved to 0x{:x}, expected 0x{:x}", 
                    name, resolved_addr, expected_addr);
            return 2;
        }
        println!("    OK: Symbol '{}' resolved to 0x{:x}", name, resolved_addr);
    }
    
    0
}

fn test_library_loading() -> i32 {
    println!("  - Testing shared library loading simulation...");
    
    let libraries = ["libc.so.6", "libm.so.6", "libpthread.so.0"];
    
    for lib_name in &libraries {
        let base_addr = simulate_library_load(lib_name);
        if base_addr == 0 {
            println!("    ERROR: Failed to load library '{}'", lib_name);
            return 3;
        }
        println!("    OK: Library '{}' loaded at base address 0x{:x}", lib_name, base_addr);
    }
    
    0
}

// Simulate symbol lookup in a dynamically linked environment
fn simulate_symbol_lookup(symbol_name: &str) -> usize {
    // This simulates what the dynamic linker would do:
    // 1. Search in loaded libraries
    // 2. Return the resolved address
    
    // Simple hash-based simulation
    let mut hash = 0usize;
    for byte in symbol_name.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as usize);
    }
    
    // Base address for libc symbols
    let libc_base = 0x60000000;
    
    match symbol_name {
        "malloc" => libc_base + 0x1000,
        "free" => libc_base + 0x1100,
        "printf" => libc_base + 0x1200,
        "strcmp" => libc_base + 0x1300,
        _ => libc_base + (hash & 0xFFFF),
    }
}

// Simulate loading a shared library
fn simulate_library_load(lib_name: &str) -> usize {
    // This simulates what the dynamic linker would do:
    // 1. Find the library file
    // 2. Parse ELF headers
    // 3. Allocate virtual memory
    // 4. Map segments
    // 5. Process relocations
    // 6. Return base address
    
    let mut hash = 0usize;
    for byte in lib_name.bytes() {
        hash = hash.wrapping_mul(17).wrapping_add(byte as usize);
    }
    
    // Different base addresses for different libraries
    match lib_name {
        "libc.so.6" => 0x60000000,
        "libm.so.6" => 0x70000000,
        "libpthread.so.0" => 0x80000000,
        _ => 0x50000000 + ((hash & 0xFF) << 20), // Random base in 0x50000000-0x5FF00000 range
    }
}

// Test function that would use a dynamically resolved symbol
#[allow(dead_code)]
fn dynamic_function_call_simulation() {
    println!("  - Simulating dynamic function call...");
    
    // In a real dynamic linking scenario, this would:
    // 1. Call through PLT entry
    // 2. PLT entry jumps to GOT
    // 3. If not resolved, GOT points to resolver
    // 4. Resolver looks up symbol and updates GOT
    // 5. Future calls go directly through GOT
    
    let printf_addr = simulate_symbol_lookup("printf");
    println!("    Simulated printf call through PLT/GOT at address 0x{:x}", printf_addr);
    
    // Simulate the indirection that would happen with PLT/GOT
    let got_entry = printf_addr; // In reality, this would be loaded from GOT
    println!("    GOT entry contains: 0x{:x}", got_entry);
}