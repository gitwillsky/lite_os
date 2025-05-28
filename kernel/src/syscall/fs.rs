const STD_OUT: usize = 1;

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
