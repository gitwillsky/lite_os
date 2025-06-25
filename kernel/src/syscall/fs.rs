use crate::arch::sbi;
const STD_OUT: usize = 1;
const STD_IN: usize = 0;

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    match fd {
        STD_OUT => {
            let str = unsafe { core::slice::from_raw_parts(buf, len) };
            let strr = core::str::from_utf8(str).unwrap();
            print!("{}", strr);
            len as isize
        }
        _ => {
            println!("sys_write: invalid fd: {}", fd);
            -1
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
