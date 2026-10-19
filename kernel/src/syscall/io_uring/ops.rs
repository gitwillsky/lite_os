//! 带 fd 的 io_uring opcode：prep 校验、同步 NOWAIT 尝试，以及把会阻塞的请求交给 worker。
//!
//! 同步尝试与 `read(2)`/`write(2)` 走同一个读写核心（`perform_read`/`perform_write`），只多一个
//! `nowait`：会睡眠的路径改返回 `EAGAIN`，由此决定请求是立即完成还是挂起。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::worker::{self, Cancel, Only, Origin, Target};
use crate::{
    file::{OpenFileDescription, OpenFileKind},
    fs::O_NONBLOCK,
    io_uring::{
        Flow, Ring, Submitter,
        abi::{
            IOSQE_CQE_SKIP_SUCCESS, IOSQE_FIXED_FILE, OP_ASYNC_CANCEL, OP_CLOSE, OP_FSYNC,
            OP_POLL_ADD, OP_POLL_REMOVE, OP_READ, OP_READV, OP_TIMEOUT, OP_TIMEOUT_REMOVE,
            OP_WRITE, OP_WRITEV, Sqe,
        },
        fail_init,
    },
    syscall::{
        fs::{
            check_readable, check_writable, import_iovecs, perform_read, perform_write,
            positioned_read, positioned_write,
        },
        user_iovec::UserIoVec,
    },
    task::TaskControlBlock,
};

const FSYNC_DATASYNC: u32 = 1 << 0;

const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLL_ADD_MULTI: u32 = 1 << 0;
const POLL_UPDATE_EVENTS: u32 = 1 << 1;
const POLL_UPDATE_USER_DATA: u32 = 1 << 2;

const TIMEOUT_ABS: u32 = 1 << 0;
const TIMEOUT_UPDATE: u32 = 1 << 1;
const TIMEOUT_BOOTTIME: u32 = 1 << 2;
const TIMEOUT_REALTIME: u32 = 1 << 3;
const LINK_TIMEOUT_UPDATE: u32 = 1 << 4;
const TIMEOUT_ETIME_SUCCESS: u32 = 1 << 5;
const TIMEOUT_MULTISHOT: u32 = 1 << 6;
const TIMEOUT_IMMEDIATE_ARG: u32 = 1 << 7;
const TIMEOUT_UPDATE_MASK: u32 = TIMEOUT_UPDATE | LINK_TIMEOUT_UPDATE;

const CANCEL_ALL: u32 = 1 << 0;
const CANCEL_FD: u32 = 1 << 1;
const CANCEL_ANY: u32 = 1 << 2;
const CANCEL_FD_FIXED: u32 = 1 << 3;
const CANCEL_USERDATA: u32 = 1 << 4;
const CANCEL_OP: u32 = 1 << 5;

/// 提交者上下文：在 `io_uring_enter` 内运行，因此当前 task 就是提交线程。
pub(super) struct TaskSubmitter {
    pub(super) task: Arc<TaskControlBlock>,
}

impl Submitter for TaskSubmitter {
    fn fd_exists(&self, fd: i32) -> bool {
        usize::try_from(fd).is_ok_and(|index| self.task.fd_get(index).is_some())
    }

    fn issue_op(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        match sqe.opcode {
            OP_READ | OP_WRITE | OP_READV | OP_WRITEV => self.read_write(ring, sqe),
            OP_FSYNC => self.fsync(ring, sqe),
            OP_CLOSE => self.close(ring, sqe),
            OP_POLL_ADD => self.poll_add(ring, sqe),
            OP_TIMEOUT => self.timeout(ring, sqe),
            OP_POLL_REMOVE | OP_TIMEOUT_REMOVE | OP_ASYNC_CANCEL => self.cancel(ring, sqe),
            _ => unreachable!("op_supported admitted an opcode without an issue arm"),
        }
    }
}

/// 会睡眠等待就绪的 OFD 种类；其余（普通文件、已就绪的设备）同步完成。
fn can_block(ofd: &OpenFileDescription) -> bool {
    match &ofd.kind {
        OpenFileKind::Inode => false,
        // signalfd 的就绪事件依赖提交线程的待决信号，worker 无法代表它登记等待。
        OpenFileKind::SignalFd(_) => false,
        _ => true,
    }
}

/// 文件偏移是否有意义（regular 文件、可定位的设备文件）；pipe/socket 等流式文件忽略 `off`。
fn positionable(ofd: &OpenFileDescription) -> bool {
    match &ofd.kind {
        OpenFileKind::Inode => true,
        OpenFileKind::Device(file) => file.positioned().is_some(),
        _ => false,
    }
}

impl TaskSubmitter {
    /// 挂起请求的来源信息：ring、提交线程（弱引用）与完成时的 user_data。
    fn origin(&self, ring: &Arc<Ring>, sqe: &Sqe) -> Origin {
        Origin {
            ring: ring.clone(),
            task: Arc::downgrade(&self.task),
            user_data: sqe.user_data,
            skip_success: sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0,
        }
    }

    fn read_write(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        let write = matches!(sqe.opcode, OP_WRITE | OP_WRITEV);
        let vectored = matches!(sqe.opcode, OP_READV | OP_WRITEV);
        // 1. prep：与 Linux `io_prep_rw` 一致，缓冲区在提交时导入，之后用户态可复用 iovec 数组。
        //    ioprio 非零需要块层优先级，尚未实现，显式拒绝而不是忽略。
        if sqe.ioprio != 0 || sqe.pad2 != 0 || sqe.personality != 0 {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        if sqe.op_flags != 0 {
            return fail_init(ring, &sqe, errno::EOPNOTSUPP);
        }
        let (vectors, total) = if vectored {
            match import_iovecs(&self.task, sqe.addr as usize, sqe.len as usize) {
                Ok(imported) => imported,
                Err(error) => return fail_init(ring, &sqe, -error),
            }
        } else {
            let length = sqe.len as usize;
            if (sqe.addr as usize).checked_add(length).is_none() {
                return fail_init(ring, &sqe, errno::EFAULT);
            }
            (
                alloc::vec![UserIoVec {
                    base: sqe.addr as usize,
                    length,
                }],
                length,
            )
        };
        // 2. issue：fd、access 与偏移错误只体现在该 CQE 的 res 上。
        let skip = sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0;
        let finish = |result: isize| {
            ring.complete(sqe.user_data, skip, result as i32);
            Flow::Continue
        };
        if sqe.flags & IOSQE_FIXED_FILE != 0 {
            return finish(-errno::EBADF);
        }
        let Some(descriptor) = usize::try_from(sqe.fd).ok() else {
            return finish(-errno::EBADF);
        };
        let Some(ofd) = self.task.fd_get(descriptor) else {
            return finish(-errno::EBADF);
        };
        let access = if write {
            check_writable(&ofd)
        } else {
            check_readable(&ofd)
        };
        if let Err(error) = access {
            return finish(error);
        }
        // off == -1 使用并推进当前 position（IORING_FEAT_RW_CUR_POS）；其他负值非法。
        let offset = sqe.off as i64;
        if offset < -1 {
            return finish(-errno::EINVAL);
        }
        let result = if offset >= 0 && positionable(&ofd) {
            // 显式偏移：pread/pwrite 语义，不触碰共享 OFD position。
            if write {
                positioned_write(descriptor, &vectors, offset, None)
            } else {
                positioned_read(descriptor, &vectors, offset)
            }
        } else if total == 0 && vectored {
            // 与 readv/writev 一致：空 iovec 总长不观察文件。
            if write {
                self.task.account_write_result(0);
            } else {
                self.task.account_read_result(0);
            }
            0
        } else if write {
            perform_write(&self.task, &ofd, &vectors, total, true)
        } else {
            perform_read(&self.task, &ofd, &vectors, total, true)
        };
        // 3. NOWAIT 返回 EAGAIN：用户自己设置了 O_NONBLOCK 的 OFD 如实返回；否则挂起等待就绪。
        if result == -errno::EAGAIN && ofd.status_flags() & O_NONBLOCK == 0 && can_block(&ofd) {
            let parked =
                worker::read_write_request(&self.origin(ring, &sqe), ofd, vectors, total, write)
                    .and_then(worker::park);
            return match parked {
                Ok(()) => Flow::Continue,
                Err(()) => finish(-errno::ENOMEM),
            };
        }
        finish(result)
    }

    fn fsync(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        // Linux `io_fsync_prep`：addr/buf_index/splice_fd_in 必须为零，flags 只允许 DATASYNC。
        if sqe.ioprio != 0
            || sqe.personality != 0
            || sqe.addr != 0
            || sqe.buf_index != 0
            || sqe.splice_fd_in != 0
            || sqe.op_flags & !FSYNC_DATASYNC != 0
        {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        let skip = sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0;
        let result = match usize::try_from(sqe.fd) {
            Ok(descriptor) if sqe.flags & IOSQE_FIXED_FILE == 0 => {
                crate::syscall::fs::sync_file(descriptor)
            }
            _ => -errno::EBADF,
        };
        ring.complete(sqe.user_data, skip, result as i32);
        Flow::Continue
    }

    fn close(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        // Linux `io_close_prep`：除 fd 外的字段必须为零；固定文件引用在 prep 阶段即 EBADF。
        if sqe.ioprio != 0
            || sqe.personality != 0
            || sqe.off != 0
            || sqe.addr != 0
            || sqe.len != 0
            || sqe.op_flags != 0
            || sqe.buf_index != 0
        {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        if sqe.flags & IOSQE_FIXED_FILE != 0 {
            return fail_init(ring, &sqe, errno::EBADF);
        }
        // 直接描述符（file_index）需要注册文件表，尚未实现。
        if sqe.splice_fd_in != 0 {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        let skip = sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0;
        let result = usize::try_from(sqe.fd)
            .ok()
            .and_then(|descriptor| self.task.fd_close(descriptor).ok())
            .map_or(-errno::EBADF, |_| 0);
        ring.complete(sqe.user_data, skip, result as i32);
        Flow::Continue
    }

    fn poll_add(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        // Linux `io_poll_add_prep`：off/addr/buf_index 必须为零；len 里只有 MULTI 一个合法位。
        if sqe.ioprio != 0
            || sqe.personality != 0
            || sqe.buf_index != 0
            || sqe.off != 0
            || sqe.addr != 0
            || sqe.len & !POLL_ADD_MULTI != 0
        {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        // multishot 需要“一个请求产生多个 CQE”（CQE_F_MORE）和重新布防，尚未实现。
        if sqe.len & POLL_ADD_MULTI != 0 {
            return fail_init(ring, &sqe, errno::EOPNOTSUPP);
        }
        let skip = sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0;
        let finish = |result: isize| {
            ring.complete(sqe.user_data, skip, result as i32);
            Flow::Continue
        };
        if sqe.flags & IOSQE_FIXED_FILE != 0 {
            return finish(-errno::EBADF);
        }
        let Some(ofd) = usize::try_from(sqe.fd)
            .ok()
            .and_then(|descriptor| self.task.fd_get(descriptor))
        else {
            return finish(-errno::EBADF);
        };
        // ERR 与 HUP 总是关注（Linux `IO_POLL_UNMASK`）；高位的 EPOLLET/ONESHOT 等由本实现的一次性语义覆盖。
        let mask = (sqe.op_flags & 0xffff) as u16 as i16 | POLLERR | POLLHUP;
        // 先 poll 一次：已就绪就同步完成，与 Linux 的 `vfs_poll` 首次探测一致。
        let ready = ofd.poll_events(mask, self.task.as_ref());
        if ready != 0 {
            return finish(isize::from(ready));
        }
        let parked =
            worker::poll_request(&self.origin(ring, &sqe), ofd, mask).and_then(worker::park);
        match parked {
            Ok(()) => Flow::Continue,
            Err(()) => finish(-errno::ENOMEM),
        }
    }

    fn timeout(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        // Linux `__io_timeout_prep`。
        if sqe.ioprio != 0
            || sqe.personality != 0
            || sqe.addr3 != 0
            || sqe.pad2 != 0
            || sqe.buf_index != 0
            || sqe.len != 1
            || sqe.splice_fd_in != 0
        {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        let flags = sqe.op_flags;
        let known = TIMEOUT_ABS
            | TIMEOUT_BOOTTIME
            | TIMEOUT_REALTIME
            | TIMEOUT_ETIME_SUCCESS
            | TIMEOUT_MULTISHOT
            | TIMEOUT_IMMEDIATE_ARG;
        if flags & !known != 0
            || (flags & (TIMEOUT_BOOTTIME | TIMEOUT_REALTIME)).count_ones() > 1
            // multishot 只对相对时间有意义
            || flags & (TIMEOUT_MULTISHOT | TIMEOUT_ABS) == TIMEOUT_MULTISHOT | TIMEOUT_ABS
        {
            return fail_init(ring, &sqe, errno::EINVAL);
        }
        // REALTIME 需要墙钟偏移；MULTISHOT 需要一个请求多个 CQE。BOOTTIME 与单调时钟在本内核相同。
        if flags & (TIMEOUT_REALTIME | TIMEOUT_MULTISHOT) != 0 {
            return fail_init(ring, &sqe, errno::EOPNOTSUPP);
        }
        let nanoseconds = if flags & TIMEOUT_IMMEDIATE_ARG != 0 {
            if (sqe.addr as i64) < 0 {
                return fail_init(ring, &sqe, errno::EINVAL);
            }
            sqe.addr
        } else {
            let mut raw = [0u8; 16];
            if self
                .task
                .copy_from_user(sqe.addr as usize, &mut raw)
                .is_err()
            {
                return fail_init(ring, &sqe, errno::EFAULT);
            }
            let seconds = i64::from_ne_bytes(raw[..8].try_into().expect("eight bytes"));
            let fraction = i64::from_ne_bytes(raw[8..].try_into().expect("eight bytes"));
            if seconds < 0 || fraction < 0 {
                return fail_init(ring, &sqe, errno::EINVAL);
            }
            (seconds as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(fraction as u64)
        };
        let deadline = if flags & TIMEOUT_ABS != 0 {
            nanoseconds
        } else {
            crate::timer::get_time_ns().saturating_add(nanoseconds)
        };
        // sqe->off 是 u32（Linux `u32 off = READ_ONCE(sqe->off)`）。
        let count = sqe.off as u32;
        let parked = worker::timeout_request(
            &self.origin(ring, &sqe),
            deadline,
            (count != 0).then(|| ring.events_posted().wrapping_add(count)),
            flags & TIMEOUT_ETIME_SUCCESS != 0,
        )
        .and_then(worker::park);
        match parked {
            Ok(()) => Flow::Continue,
            Err(()) => {
                ring.complete(sqe.user_data, false, -(errno::ENOMEM as i32));
                Flow::Continue
            }
        }
    }

    /// `POLL_REMOVE`、`TIMEOUT_REMOVE` 与 `ASYNC_CANCEL`：目标请求由 worker 持有，所以取消也交给
    /// worker 执行并由它发布两个 CQE（目标的 `-ECANCELED` 与本请求的结果）。
    fn cancel(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow {
        let (target, all) = match sqe.opcode {
            OP_TIMEOUT_REMOVE => {
                // Linux `io_timeout_remove_prep`：除 addr（目标 user_data）外字段为零，更新类 flags 未实现。
                if sqe.flags & IOSQE_FIXED_FILE != 0
                    || sqe.ioprio != 0
                    || sqe.personality != 0
                    || sqe.addr3 != 0
                    || sqe.pad2 != 0
                    || sqe.buf_index != 0
                    || sqe.len != 0
                    || sqe.splice_fd_in != 0
                {
                    return fail_init(ring, &sqe, errno::EINVAL);
                }
                if sqe.op_flags & TIMEOUT_UPDATE_MASK != 0 {
                    return fail_init(ring, &sqe, errno::EOPNOTSUPP);
                }
                if sqe.op_flags != 0 {
                    return fail_init(ring, &sqe, errno::EINVAL);
                }
                (Target::UserData(sqe.addr, Only::Timeout), false)
            }
            OP_POLL_REMOVE => {
                // Linux `io_poll_remove_prep`。
                if sqe.ioprio != 0
                    || sqe.personality != 0
                    || sqe.buf_index != 0
                    || sqe.splice_fd_in != 0
                {
                    return fail_init(ring, &sqe, errno::EINVAL);
                }
                let flags = sqe.len;
                if flags & !(POLL_UPDATE_EVENTS | POLL_UPDATE_USER_DATA | POLL_ADD_MULTI) != 0
                    || flags == POLL_ADD_MULTI
                    || sqe.off != 0
                    || sqe.op_flags != 0
                {
                    return fail_init(ring, &sqe, errno::EINVAL);
                }
                if flags != 0 {
                    return fail_init(ring, &sqe, errno::EOPNOTSUPP);
                }
                (Target::UserData(sqe.addr, Only::Poll), false)
            }
            OP_ASYNC_CANCEL => {
                // Linux `io_async_cancel_prep`。
                if sqe.ioprio != 0 || sqe.personality != 0 || sqe.off != 0 || sqe.splice_fd_in != 0
                {
                    return fail_init(ring, &sqe, errno::EINVAL);
                }
                let flags = sqe.op_flags;
                if flags
                    & !(CANCEL_ALL
                        | CANCEL_FD
                        | CANCEL_ANY
                        | CANCEL_FD_FIXED
                        | CANCEL_USERDATA
                        | CANCEL_OP)
                    != 0
                    || (flags & CANCEL_FD != 0 && flags & CANCEL_ANY != 0)
                    || (flags & CANCEL_OP != 0 && flags & CANCEL_ANY != 0)
                {
                    return fail_init(ring, &sqe, errno::EINVAL);
                }
                // 按 fd / opcode 匹配需要请求记录它们的来源，尚未实现。
                if flags & (CANCEL_FD | CANCEL_FD_FIXED | CANCEL_OP) != 0 {
                    return fail_init(ring, &sqe, errno::EOPNOTSUPP);
                }
                let target = if flags & CANCEL_ANY != 0 {
                    Target::Any
                } else {
                    Target::UserData(sqe.addr, Only::Any)
                };
                (target, flags & CANCEL_ALL != 0)
            }
            _ => unreachable!("cancel handles exactly the three cancel-style opcodes"),
        };
        let command = Cancel {
            ring: ring.clone(),
            user_data: sqe.user_data,
            skip_success: sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0,
            target,
            all,
        };
        if worker::cancel(command).is_err() {
            ring.complete(sqe.user_data, false, -(errno::ENOMEM as i32));
        }
        Flow::Continue
    }
}
