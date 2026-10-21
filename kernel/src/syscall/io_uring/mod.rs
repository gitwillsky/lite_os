//! `io_uring_setup`/`io_uring_enter`/`io_uring_register` 的 ABI 解码、user-copy 与 errno。
//!
//! ring 的布局、SQE 校验和 CQE 发布由 `crate::io_uring` 拥有；这里不解释 ring 内存。

mod chain;
mod exec;
mod lane;
mod ops;
mod wait;
mod worker;

use alloc::{sync::Arc, vec::Vec};
use syscall_abi::errno;

/// 启动 io_uring 的两个内核线程：reactor（多路复用挂起的链）与阻塞车道（可能睡眠在磁盘上的作业）。
pub(crate) fn start_worker() -> Result<(), ()> {
    worker::start()?;
    lane::start()
}

use crate::{
    file::OpenFileKind,
    io_uring::{
        CqWait, Ring, SetupFailure,
        abi::{
            ENTER_EXT_ARG, ENTER_GETEVENTS, GETEVENTS_ARG_BYTES, PARAMS_BYTES, PROBE_HEADER_BYTES,
            PROBE_OP_BYTES, REGISTER_PROBE, SUPPORTED_ENTER_FLAGS, encode_probe, probe_ops,
        },
    },
    task::current_task,
};

/// 创建 ring 与对应 fd；fd 恒带 `O_CLOEXEC`（Linux `get_unused_fd_flags(O_RDWR | O_CLOEXEC)`）。
///
/// # Returns
///
/// 新 fd；参数非法 `EINVAL`，用户指针不可访问 `EFAULT`，内存不足 `ENOMEM`，fd 表满 `EMFILE`。
pub(crate) fn sys_io_uring_setup(entries: u32, params: usize) -> isize {
    let task = current_task().expect("io_uring_setup requires current task");
    let mut input = [0u8; PARAMS_BYTES];
    if task.copy_from_user(params, &mut input).is_err() {
        return -errno::EFAULT;
    }
    let reservation = match task.fd_prepare_open(true) {
        Ok(reservation) => reservation,
        Err(error) => return super::file_descriptor_error(error),
    };
    let prepared = match crate::file::PreparedDeviceDescription::try_new(
        crate::io_uring::ANONYMOUS_NAME,
        crate::fs::O_RDWR,
    ) {
        Ok(prepared) => prepared,
        Err(_) => return -errno::ENOMEM,
    };
    let (ring, output) = match crate::io_uring::create(entries, &input, task.user_resource_owner())
    {
        Ok(created) => created,
        Err(SetupFailure::Invalid) => return -errno::EINVAL,
        Err(SetupFailure::OutOfMemory) => return -errno::ENOMEM,
    };
    // 先写回 params 再发布 fd：发布后失败需要回收已安装的 fd，先复制则失败时只丢弃 reservation。
    if task.copy_to_user(params, &output).is_err() {
        return -errno::EFAULT;
    }
    reservation.publish(prepared.finish(ring)) as isize
}

/// 取出 ring；fd 无效 `EBADF`，不是 io_uring fd `EOPNOTSUPP`（与 Linux 相同）。
fn ring_of(task: &crate::task::TaskControlBlock, fd: usize) -> Result<Arc<Ring>, isize> {
    let Some(file) = task.fd_get(fd) else {
        return Err(-errno::EBADF);
    };
    match &file.kind {
        OpenFileKind::Device(backend) => {
            crate::io_uring::ring_arc_of(backend).ok_or(-errno::EOPNOTSUPP)
        }
        _ => Err(-errno::EOPNOTSUPP),
    }
}

/// 提交至多 `to_submit` 个 SQE，并按 `IORING_ENTER_GETEVENTS` 等待完成。
///
/// # Returns
///
/// 提交数；没有提交时返回等待结果（0、`ETIME`、`EINTR`、`EBADR`）。
pub(crate) fn sys_io_uring_enter(
    fd: usize,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    argp: usize,
    argsz: usize,
) -> isize {
    if flags & !SUPPORTED_ENTER_FLAGS != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("io_uring_enter requires current task");
    let ring = match ring_of(&task, fd) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let mut result: isize = 0;
    if to_submit != 0 {
        let submitter = ops::TaskSubmitter { task: task.clone() };
        match crate::io_uring::submit(&ring, to_submit, &submitter) {
            Ok(submitted) => {
                result = submitted as isize;
                if submitted != to_submit {
                    return result;
                }
            }
            Err(_) => return -errno::ENOMEM,
        }
    }
    if flags & ENTER_GETEVENTS != 0 {
        let waited = get_events(&task, &ring, min_complete, flags, argp, argsz);
        if result == 0 {
            result = waited;
        }
    }
    result
}

/// 解析 `argp`（sigset 指针或 `io_uring_getevents_arg`）并等待 CQE。
fn get_events(
    task: &crate::task::TaskControlBlock,
    ring: &Ring,
    min_complete: u32,
    flags: u32,
    argp: usize,
    argsz: usize,
) -> isize {
    let (sigmask, sigmask_size, deadline) = if flags & ENTER_EXT_ARG == 0 {
        (argp, argsz, None)
    } else {
        if argsz != GETEVENTS_ARG_BYTES {
            return -errno::EINVAL;
        }
        let mut raw = [0u8; GETEVENTS_ARG_BYTES];
        if task.copy_from_user(argp, &mut raw).is_err() {
            return -errno::EFAULT;
        }
        let word64 = |o: usize| u64::from_ne_bytes(raw[o..o + 8].try_into().expect("eight bytes"));
        let word32 = |o: usize| u32::from_ne_bytes(raw[o..o + 4].try_into().expect("four bytes"));
        // min_wait_usec 要求“先等够 min_wait 再按批返回”的额外调度语义，尚未实现；静默忽略会改变
        // 等待行为，所以显式拒绝。
        if word32(12) != 0 {
            return -errno::EINVAL;
        }
        let deadline = match word64(16) {
            0 => None,
            timespec => match relative_deadline(task, timespec as usize) {
                Ok(deadline) => Some(deadline),
                Err(error) => return error,
            },
        };
        (word64(0) as usize, word32(8) as usize, deadline)
    };
    let temporary_mask = sigmask != 0;
    if temporary_mask {
        if sigmask_size != 8 {
            return -errno::EINVAL;
        }
        let mut bytes = [0u8; 8];
        if task.copy_from_user(sigmask, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        task.begin_signal_suspend(u64::from_ne_bytes(bytes));
    }
    let outcome = ring.wait_for(min_complete, deadline);
    // EINTR 时 signal frame 必须看到 begin_signal_suspend 保存的旧 mask，不能抢先恢复。
    if temporary_mask && outcome != CqWait::Interrupted {
        task.restore_temporary_signal_mask()
            .expect("temporary signal mask was installed by this call");
    }
    match outcome {
        CqWait::Satisfied => 0,
        CqWait::Dropped => -errno::EBADR,
        CqWait::TimedOut => -errno::ETIME,
        CqWait::Interrupted => -errno::EINTR,
        CqWait::OutOfMemory => -errno::ENOMEM,
    }
}

/// 读取用户 `__kernel_timespec` 并换算成绝对 monotonic 纳秒 deadline。
fn relative_deadline(task: &crate::task::TaskControlBlock, pointer: usize) -> Result<u64, isize> {
    let mut raw = [0u8; 16];
    if task.copy_from_user(pointer, &mut raw).is_err() {
        return Err(-errno::EFAULT);
    }
    let seconds = i64::from_ne_bytes(raw[..8].try_into().expect("eight bytes"));
    let nanoseconds = i64::from_ne_bytes(raw[8..16].try_into().expect("eight bytes"));
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(-errno::EINVAL);
    }
    let relative = (seconds as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nanoseconds as u64);
    Ok(crate::timer::get_time_ns().saturating_add(relative))
}

/// 目前只实现 `IORING_REGISTER_PROBE`；其余 opcode 返回 `EINVAL`。
pub(crate) fn sys_io_uring_register(fd: usize, opcode: u32, arg: usize, nr_args: u32) -> isize {
    let task = current_task().expect("io_uring_register requires current task");
    if let Err(error) = ring_of(&task, fd) {
        return error;
    }
    match opcode {
        REGISTER_PROBE => probe(&task, arg, nr_args),
        _ => -errno::EINVAL,
    }
}

/// Linux `io_probe`：用户缓冲区必须整体清零，响应逐 opcode 标注支持情况。
fn probe(task: &crate::task::TaskControlBlock, arg: usize, nr_args: u32) -> isize {
    let ops = probe_ops(nr_args);
    let size = PROBE_HEADER_BYTES + ops * PROBE_OP_BYTES;
    let mut buffer = Vec::new();
    if buffer.try_reserve_exact(size).is_err() {
        return -errno::ENOMEM;
    }
    buffer.resize(size, 0);
    if task.copy_from_user(arg, &mut buffer).is_err() {
        return -errno::EFAULT;
    }
    if buffer.iter().any(|byte| *byte != 0) {
        return -errno::EINVAL;
    }
    encode_probe(ops, &mut buffer);
    if task.copy_to_user(arg, &buffer).is_err() {
        return -errno::EFAULT;
    }
    0
}
