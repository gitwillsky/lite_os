#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{sched_yield, write};

#[unsafe(no_mangle)]
extern "C" fn main(_argc: usize, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let _ = write(1, b"LiteOS init\n");
    loop {
        let _ = sched_yield();
    }
}
