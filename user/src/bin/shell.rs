#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
use user_lib::sys_read;

fn read_line(buf: &mut [u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let mut byte = [0u8; 1];
        sys_read(0, &mut byte);
        if byte[0] == b'\n' || byte[0] == b'\r' {
            break;
        }
        buf[i] = byte[0];
        i += 1;
    }
    i
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("[user shell] User shell program started");
    println!("[user shell] shell main entry");
    let mut buf = [0u8; 128];
    loop {
        print!("$ ");
        let len = read_line(&mut buf);
        if len == 0 {
            continue;
        }
        let cmd = core::str::from_utf8(&buf[..len]).unwrap_or("");
        match cmd.trim() {
            "hello" => {
                println!("Hello from shell!");
            }
            "exit" => {
                println!("Bye!");
                break;
            }
            s => {
                print!("{}",s)
            }
        }
    }
    println!("[user shell] Shell program about to exit");
    0
}

