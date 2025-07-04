#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
use user_lib::sys_read;

const LF: u8 = b'\n';
const CR: u8 = b'\r';
const DL: u8 = b'\x7f'; // DEL
const BS: u8 = b'\x08'; // BACKSPACE

fn read_line(buf: &mut [u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let mut byte = [0u8; 1];
        if sys_read(0, &mut byte) <= 0 {
            // 如果没有输入，可以稍微等待一下，避免CPU空转
            // 在更高级的实现中，这里应该是阻塞或yield
            continue;
        }

        let c = byte[0];
        match c {
            CR | LF => {
                print!("\n");
                break;
            }
            BS | DL => {
                if i > 0 {
                    i -= 1;
                    // 在控制台上实现退格效果
                    print!("\x08 \x08");
                }
            }
            _ => {
                buf[i] = c;
                i += 1;
                print!("{}", c as char);
            }
        }
    }
    i
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut buf = [0u8; 128];
    loop {
        print!("$ ");
        let len = read_line(&mut buf);
        if len == 0 {
            continue;
        }
        let cmd = core::str::from_utf8(&buf[..len]).unwrap_or("").trim();
        match cmd {
            "" => {
                // 用户只按了回车，read_line 已经处理了换行，所以这里什么都不用做
            }
            "hello" => {
                println!("Hello from shell!");
            }
            "exit" => {
                println!("Bye!");
                break;
            }
            s => {
                println!("{}: command not found", s);
            }
        }
        // 为下一次循环清空缓冲区，这是一个好的编程实践
        buf.fill(0);
    }
    println!("[user shell] Shell program about to exit");
    0
}
