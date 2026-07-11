use crate::{
    memory::page_table::translated_byte_buffer,
    syscall::errno::{EFAULT, EINVAL},
    task::current_user_token,
};

/// @description Linux/riscv64 `timespec` 的最小 64 位布局。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: u64,
    pub tv_nsec: u64,
}

/// @description 按相对单调时间挂起当前任务。
///
/// @param req 用户态请求时间；空指针返回 `EINVAL`。
/// @param rem 剩余时间输出地址；当前实现尚不支持中断剩余时间。
/// @return 成功返回零，失败返回负 errno。
pub fn sys_nanosleep(req: *const TimeSpec, _rem: *mut TimeSpec) -> isize {
    if req.is_null() {
        return -EINVAL;
    }

    let buffers = translated_byte_buffer(
        current_user_token(),
        req.cast::<u8>(),
        core::mem::size_of::<TimeSpec>(),
    );
    if buffers.is_empty() || buffers[0].len() < core::mem::size_of::<TimeSpec>() {
        return -EFAULT;
    }

    let timespec = unsafe { (buffers[0].as_ptr() as *const TimeSpec).read_unaligned() };
    if timespec.tv_nsec >= 1_000_000_000 {
        return -EINVAL;
    }
    let Some(total_ns) = timespec
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(timespec.tv_nsec))
    else {
        return -EINVAL;
    };

    if total_ns == 0 {
        0
    } else {
        crate::task::nanosleep(total_ns)
    }
}
