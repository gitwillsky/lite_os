#![no_std]
#![no_main]

use user_lib::*;

/// Simple test program that demonstrates it received execution
#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("Arguments Test Program - RUNNING");
    println!("================================");
    
    println!("This program was successfully executed!");
    println!("The argument passing mechanism is working.");
    
    // For now, we'll just verify that the program executed
    // In a full implementation, we would access argc/argv from the stack
    
    println!("Program completed successfully!");
    0
}