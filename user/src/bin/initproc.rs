#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exec, fork, wait, yield_};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut shell_pid = None;

    // Start initial shell
    spawn_shell(&mut shell_pid);

    // Main process reaping loop
    loop {
        let mut exit_code: i32 = 0;
        let exited_pid = wait(&mut exit_code);

        if exited_pid == -1 {
            yield_();
            continue;
        }

        // Check if the shell exited
        if let Some(current_shell_pid) = shell_pid {
            if exited_pid as usize == current_shell_pid {
                shell_pid = None;
                spawn_shell(&mut shell_pid);
            }
        }
    }
}

fn spawn_shell(shell_pid: &mut Option<usize>) {
    let pid = fork();
    if pid == 0 {
        exec("user_shell\0");
        user_lib::exit(1);
    } else if pid > 0 {
        *shell_pid = Some(pid as usize);
    } else {
        println!("initproc: failed to fork shell process");
    }
}
