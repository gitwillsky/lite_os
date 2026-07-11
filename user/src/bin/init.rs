#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::sched_yield;

#[unsafe(no_mangle)]
fn main() -> i32 {
    loop {
        let _ = sched_yield();
    }
}
