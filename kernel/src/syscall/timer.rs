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

    let token = current_user_token();
    let req_buffers =
        translated_byte_buffer(token, req as *const u8, core::mem::size_of::<TimeSpec>());

    if req_buffers.is_empty() {
        return -14; // EFAULT: bad address
    }

    let timespec = unsafe { *(req_buffers[0].as_ptr() as *const TimeSpec) };

    if timespec.tv_nsec >= 1000_000_000 {
        return -22; // EINVAL: invalid nanoseconds
    }

    let total_nanoseconds = timespec.tv_sec * 1_000_000_000 + timespec.tv_nsec;

    if total_nanoseconds == 0 {
        return 0;
    }

    crate::task::nanosleep(total_nanoseconds)
}

pub fn sys_time() -> isize {
    timer::get_unix_timestamp() as isize
}

pub fn sys_gettimeofday(tv: *mut TimeVal, tz: *mut u8) -> isize {
    if tv.is_null() {
        return -22; // EINVAL
    }

    let unix_timestamp_us = timer::get_unix_timestamp_us();
    let seconds = unix_timestamp_us / 1_000_000;
    let microseconds = unix_timestamp_us % 1_000_000;

    let timeval = TimeVal { tv_sec: seconds, tv_usec: microseconds };

    let token = current_user_token();
    let mut tv_buffers = translated_byte_buffer(token, tv as *const u8, core::mem::size_of::<TimeVal>());

    if tv_buffers.is_empty() {
        return -14;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(
            &timeval as *const TimeVal as *const u8,
            tv_buffers[0].as_mut_ptr(),
            core::mem::size_of::<TimeVal>(),
        );
    }

    0
}

#[repr(C)]
struct LinuxTimespec { tv_sec: i64, tv_nsec: i64 }

pub fn sys_clock_gettime(clock_id: i32, tp: *mut u8) -> isize {
    if tp.is_null() { return -14; }
    let (sec, nsec) = match clock_id {
        0 => { let us = crate::timer::get_unix_timestamp_us(); (us / 1_000_000, (us % 1_000_000) * 1000) },
        1 => { let ns = crate::timer::get_time_ns() as u128; ((ns / 1_000_000_000) as u64, (ns % 1_000_000_000) as u64) },
        _ => return -22,
    };
    let ts = LinuxTimespec { tv_sec: sec as i64, tv_nsec: nsec as i64 };
    let token = current_user_token();
    let mut bufs = translated_byte_buffer(token, tp as *const u8, core::mem::size_of::<LinuxTimespec>());
    if bufs.is_empty() || bufs[0].len() < core::mem::size_of::<LinuxTimespec>() { return -14; }
    unsafe {
        core::ptr::copy_nonoverlapping(
            &ts as *const LinuxTimespec as *const u8,
            bufs[0].as_mut_ptr(),
            core::mem::size_of::<LinuxTimespec>(),
        );
    }
    0
}
