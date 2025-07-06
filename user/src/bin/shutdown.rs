#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::shutdown;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("Shutting down...");
    shutdown();
    0
}
