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
            let buffers = translated_byte_buffer(current_user_token(), buf, len);
            let mut read_len = 0;

            for buffer in buffers {
                for i in 0..buffer.len() {
                    let ch = loop {
                        let c = sbi::console_getchar();
                        if c >= 0 {
                            break c as u8;
                        }
                    };

                    buffer[i] = ch;
                    read_len += 1;

                    if ch == b'\n' || ch == b'\r' {
                        return read_len as isize;
                    }
                }
            }
            read_len as isize
        }
        _ => {
            println!("sys_read: invalid fd: {}", fd);
            -1
        }
    }
}
