use crate::memory::page_table::translated_byte_buffer;
use crate::task::current_user_token;
use crate::timer;

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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeVal {
    pub tv_sec: u64,  // 秒
    pub tv_usec: u64, // 微秒
}

pub fn sys_nanosleep(req: *const TimeSpec, rem: *mut TimeSpec) -> isize {
    if req.is_null() {
        return -22; // EINVAL
    }

    // 安全地从用户空间读取TimeSpec结构
    let token = current_user_token();
    let req_buffers =
        translated_byte_buffer(token, req as *const u8, core::mem::size_of::<TimeSpec>());

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
    crate::task::nanosleep(total_nanoseconds)
}

// 获取 Unix 时间戳（秒）
pub fn sys_time() -> isize {
    timer::get_unix_timestamp() as isize
}

// 获取当前时间和时区信息 (POSIX gettimeofday)
pub fn sys_gettimeofday(tv: *mut TimeVal, tz: *mut u8) -> isize {
    if tv.is_null() {
        return -22; // EINVAL
    }

    // 获取真实的 Unix 时间戳
    let unix_timestamp_us = timer::get_unix_timestamp_us();
    let seconds = unix_timestamp_us / 1_000_000;
    let microseconds = unix_timestamp_us % 1_000_000;

    let timeval = TimeVal {
        tv_sec: seconds,
        tv_usec: microseconds,
    };

    // 安全地写入用户空间
    let token = current_user_token();
    let mut tv_buffers =
        translated_byte_buffer(token, tv as *const u8, core::mem::size_of::<TimeVal>());

    if tv_buffers.is_empty() {
        return -14; // EFAULT: bad address
    }

    // 写入 TimeVal 结构
    unsafe {
        core::ptr::copy_nonoverlapping(
            &timeval as *const TimeVal as *const u8,
            tv_buffers[0].as_mut_ptr(),
            core::mem::size_of::<TimeVal>(),
        );
    }

    // 忽略时区参数（在现代系统中通常为 null）
    // 如果需要可以在此处处理时区信息

    0 // 成功
}
