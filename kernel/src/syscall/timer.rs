use crate::timer;
use crate::memory::page_table::translated_byte_buffer;
use crate::task::current_user_token;

pub fn sys_get_time_msec() -> isize {
    timer::get_time_msec() as isize
}

pub fn sys_get_time_us() -> isize {
    timer::get_time_us() as isize
}

pub fn sys_get_time_ns() -> isize {
    timer::get_time_ns() as isize
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: u64,  // 秒
    pub tv_nsec: u64, // 纳秒
}

pub fn sys_nanosleep(req: *const TimeSpec, rem: *mut TimeSpec) -> isize {
    if req.is_null() {
        return -22; // EINVAL
    }

    // 安全地从用户空间读取TimeSpec结构
    let token = current_user_token();
    let req_buffers = translated_byte_buffer(token, req as *const u8, core::mem::size_of::<TimeSpec>());
    
    if req_buffers.is_empty() {
        return -14; // EFAULT: bad address
    }
    
    // 从缓冲区中读取TimeSpec
    let timespec = unsafe { *(req_buffers[0].as_ptr() as *const TimeSpec) };

    // 参数验证
    if timespec.tv_nsec >= 1000_000_000 {
        return -22; // EINVAL: invalid nanoseconds
    }

    // 转换为纳秒
    let total_nanoseconds = timespec.tv_sec * 1_000_000_000 + timespec.tv_nsec;

    if total_nanoseconds == 0 {
        return 0; // 无需睡眠
    }

    // 调用内核睡眠函数
    timer::nanosleep(total_nanoseconds)
}
