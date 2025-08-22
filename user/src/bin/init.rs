#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use user_lib::{exec, exit, fork, wait, yield_};

// GUI Splash 逻辑移除，交由独立合成器 litewm 负责

#[unsafe(no_mangle)]
fn main() -> i32 {
    // spawn_text_test();
    spawn_webwm();
    spawn_shell();

    // Main process reaping loop
    loop {
        let mut exit_code: i32 = 0;
        let exited_pid = wait(&mut exit_code);

        if exited_pid == -1 {
            yield_();
            continue;
        }
    }
}

fn spawn_shell() {
    let pid = fork();
    if pid == 0 {
        let exit_code = exec("/bin/shell") as i32;
        exit(exit_code);
    } else if pid > 0 {
        // shell started
    } else {
        println!("init: failed to fork shell process");
    }
}

fn spawn_webwm() {
    let pid = fork();
    if pid == 0 {
        let exit_code = exec("/bin/webwm") as i32;
        exit(exit_code);
    } else if pid > 0 {
    } else {
        println!("init: failed to fork webwm process");
    }
}

fn spawn_text_test() {
    let pid = fork();
    if pid == 0 {
        let exit_code = exec("/text_test") as i32;
        exit(exit_code);
    } else if pid > 0 {
    } else {
        println!("init: failed to fork text_test process");
    }
}

