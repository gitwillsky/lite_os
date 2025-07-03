use crate::{arch::sbi, memory::page_table::translated_byte_buffer, task::current_user_token};
const STD_OUT: usize = 1;
const STD_IN: usize = 0;


/// write buf of length `len`  to a file with `fd`
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    match fd {
        STD_OUT => {
            let buffers = translated_byte_buffer(current_user_token(), buf, len);
            for buffer in buffers {
                let s = core::str::from_utf8(buffer).unwrap();
                for c in s.bytes() {
                    sbi::console_putchar(c as usize);
                }
            }
            len as isize
        }
        _ => {
            panic!("Unsupported fd in sys_write!");
        }
    }
}


pub fn sys_read(fd: usize, buf: *mut u8, len: usize) -> isize {
    match fd {
        STD_IN => {
            let mut read = 0;
            for i in 0..len {
                // 轮询获取一个字符
                let c = loop {
                    let ch = sbi::console_getchar();
                    if ch >= 0 {
                        break ch as u8;
                    }
                };
                unsafe {
                    *buf.add(i) = c;
                }
                read += 1;
                if c == b'\n' || c == b'\r' {
                    break;
                }
            }
            read
        }
        _ => {
            println!("sys_read: invalid fd: {}", fd);
            -1
        }
    }
}
