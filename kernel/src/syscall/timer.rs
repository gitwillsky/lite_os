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

pub fn sys_nanosleep(req: *const TimeSpec, rem: *mut TimeSpec) -> isize {
    if req.is_null() {
        return -22; // EINVAL
    }
    
    let timespec = unsafe { *req };
    
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
