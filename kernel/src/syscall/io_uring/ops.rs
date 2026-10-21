//! 带 fd 的 io_uring opcode 的 prep：校验 SQE 并解码成拥有全部用户数据的 [`Op`]。
//!
//! prep 在提交者上下文里运行（读取用户内存、拷贝 iovec），不解析 fd——fd 在 issue 时才对 fd 表求值
//! （见 `exec`）。执行与链的推进见 `chain`。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{
    chain::launch,
    exec::Op,
    wait::{Only, Target},
};
use crate::{
    io_uring::{
        Chain, Ring, Submitter,
        abi::{
            IOSQE_FIXED_FILE, OP_ACCEPT, OP_ASYNC_CANCEL, OP_CLOSE, OP_CONNECT, OP_FSYNC,
            OP_LINK_TIMEOUT, OP_OPENAT, OP_POLL_ADD, OP_POLL_REMOVE, OP_READ, OP_READV, OP_RECV,
            OP_SEND, OP_TIMEOUT, OP_TIMEOUT_REMOVE, OP_WRITE, OP_WRITEV, Sqe,
        },
    },
    syscall::{
        fs::import_iovecs,
        socket::{MSG_DONTWAIT, MSG_NOSIGNAL, SOCK_CLOEXEC, SOCK_NONBLOCK},
        user_iovec::UserIoVec,
    },
    task::TaskControlBlock,
};

const FSYNC_DATASYNC: u32 = 1 << 0;

const ACCEPT_MULTISHOT: u16 = 1 << 0;

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
    type Op = Op;

    fn prepare_op(&self, _ring: &Ring, sqe: &Sqe) -> Result<Op, isize> {
        match sqe.opcode {
            OP_READ | OP_WRITE | OP_READV | OP_WRITEV => self.read_write(sqe),
            OP_FSYNC => fsync(sqe),
            OP_CLOSE => close(sqe),
            OP_OPENAT => self.openat(sqe),
            OP_POLL_ADD => poll_add(sqe),
            OP_ACCEPT => accept(sqe),
            OP_CONNECT => connect(sqe),
            OP_SEND | OP_RECV => self.send_recv(sqe),
            OP_TIMEOUT => self.timeout(sqe),
            OP_LINK_TIMEOUT => self.link_timeout(sqe),
            OP_POLL_REMOVE | OP_TIMEOUT_REMOVE | OP_ASYNC_CANCEL => cancel(sqe),
            _ => unreachable!("op_supported admitted an opcode without a prep arm"),
        }
    }

    fn launch(&self, ring: &Arc<Ring>, chain: Chain<Op>) {
        launch(&self.task, ring, chain);
    }
}

/// `sqe->fd` 解析成 fd 表下标；负数或 fixed 引用（没有注册文件表）在 issue 时得到 `EBADF`。
fn fd_of(sqe: &Sqe) -> Option<usize> {
    if sqe.flags & IOSQE_FIXED_FILE != 0 {
        return None;
    }
    usize::try_from(sqe.fd).ok()
}

impl TaskSubmitter {
    /// `IORING_OP_OPENAT`（Linux `__io_openat_prep`）：路径在 prep 时复制，之后用户态改写缓冲区不影响本次 open。
    fn openat(&self, sqe: &Sqe) -> Result<Op, isize> {
        if sqe.buf_index != 0 {
            return Err(errno::EINVAL);
        }
        if sqe.flags & IOSQE_FIXED_FILE != 0 {
            return Err(errno::EBADF);
        }
        // `file_index` 非零是直接描述符（把结果装进注册文件表），本内核没有注册文件表。
        if sqe.splice_fd_in != 0 {
            return Err(errno::EOPNOTSUPP);
        }
        let path = crate::syscall::fs::user_path(&self.task, sqe.addr as *const u8)
            .map_err(|error| -error)?;
        Ok(Op::OpenAt {
            dirfd: sqe.fd as isize,
            path,
            flags: sqe.op_flags,
            mode: sqe.len,
        })
    }

    fn read_write(&self, sqe: &Sqe) -> Result<Op, isize> {
        let write = matches!(sqe.opcode, OP_WRITE | OP_WRITEV);
        let vectored = matches!(sqe.opcode, OP_READV | OP_WRITEV);
        // 与 Linux `io_prep_rw` 一致，缓冲区在提交时导入，之后用户态可复用 iovec 数组。
        // ioprio 非零需要块层优先级，尚未实现，显式拒绝而不是忽略。
        if sqe.ioprio != 0 || sqe.pad2 != 0 || sqe.personality != 0 {
            return Err(errno::EINVAL);
        }
        if sqe.op_flags != 0 {
            return Err(errno::EOPNOTSUPP);
        }
        let (vectors, total) = if vectored {
            import_iovecs(&self.task, sqe.addr as usize, sqe.len as usize)
                .map_err(|error| -error)?
        } else {
            let length = sqe.len as usize;
            if (sqe.addr as usize).checked_add(length).is_none() {
                return Err(errno::EFAULT);
            }
            (
                alloc::vec![UserIoVec {
                    base: sqe.addr as usize,
                    length,
                }],
                length,
            )
        };
        Ok(Op::ReadWrite {
            fd: fd_of(sqe),
            vectors,
            total,
            write,
            vectored,
            offset: sqe.off as i64,
        })
    }

    /// `IORING_OP_SEND`/`IORING_OP_RECV`：目的地址、`ioprio` 里的 recvsend flags 与 `msg_flags` 中
    /// 除 `MSG_DONTWAIT`/`MSG_NOSIGNAL` 外的位尚未支持，显式返回 `EOPNOTSUPP` 而不是忽略。
    fn send_recv(&self, sqe: &Sqe) -> Result<Op, isize> {
        if sqe.personality != 0 || sqe.addr3 != 0 || sqe.buf_index != 0 {
            return Err(errno::EINVAL);
        }
        if sqe.ioprio != 0 || sqe.off != 0 || sqe.splice_fd_in != 0 {
            return Err(errno::EOPNOTSUPP);
        }
        let flags = sqe.op_flags as usize;
        if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
            return Err(errno::EOPNOTSUPP);
        }
        let length = sqe.len as usize;
        if (sqe.addr as usize).checked_add(length).is_none() {
            return Err(errno::EFAULT);
        }
        Ok(Op::Socket {
            fd: fd_of(sqe),
            vectors: alloc::vec![UserIoVec {
                base: sqe.addr as usize,
                length,
            }],
            total: length,
            write: sqe.opcode == OP_SEND,
            dontwait: flags & MSG_DONTWAIT != 0,
        })
    }

    fn timeout(&self, sqe: &Sqe) -> Result<Op, isize> {
        // Linux `__io_timeout_prep`。
        if sqe.ioprio != 0
            || sqe.personality != 0
            || sqe.addr3 != 0
            || sqe.pad2 != 0
            || sqe.buf_index != 0
            || sqe.len != 1
            || sqe.splice_fd_in != 0
        {
            return Err(errno::EINVAL);
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
            return Err(errno::EINVAL);
        }
        // REALTIME 需要墙钟偏移；MULTISHOT 需要一个请求多个 CQE。BOOTTIME 与单调时钟在本内核相同。
        if flags & (TIMEOUT_REALTIME | TIMEOUT_MULTISHOT) != 0 {
            return Err(errno::EOPNOTSUPP);
        }
        let nanoseconds = self.timeout_nanoseconds(sqe, flags)?;
        Ok(Op::Timeout {
            nanoseconds,
            absolute: flags & TIMEOUT_ABS != 0,
            // sqe->off 是 u32（Linux `u32 off = READ_ONCE(sqe->off)`）。
            count: sqe.off as u32,
            etime_success: flags & TIMEOUT_ETIME_SUCCESS != 0,
        })
    }

    /// `sqe->addr` 指向的 `__kernel_timespec`（或 `IMMEDIATE_ARG` 的纳秒值）解码成纳秒。
    fn timeout_nanoseconds(&self, sqe: &Sqe, flags: u32) -> Result<u64, isize> {
        Ok(if flags & TIMEOUT_IMMEDIATE_ARG != 0 {
            if (sqe.addr as i64) < 0 {
                return Err(errno::EINVAL);
            }
            sqe.addr
        } else {
            let mut raw = [0u8; 16];
            self.task
                .copy_from_user(sqe.addr as usize, &mut raw)
                .map_err(|_| errno::EFAULT)?;
            let seconds = i64::from_ne_bytes(raw[..8].try_into().expect("eight bytes"));
            let fraction = i64::from_ne_bytes(raw[8..].try_into().expect("eight bytes"));
            if seconds < 0 || fraction < 0 {
                return Err(errno::EINVAL);
            }
            (seconds as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(fraction as u64)
        })
    }

    /// `IORING_OP_LINK_TIMEOUT`（Linux `__io_timeout_prep` 的 link 分支）：只允许绝对/相对与时钟选择。
    fn link_timeout(&self, sqe: &Sqe) -> Result<Op, isize> {
        if sqe.ioprio != 0
            || sqe.personality != 0
            || sqe.addr3 != 0
            || sqe.pad2 != 0
            || sqe.buf_index != 0
            || sqe.len != 1
            || sqe.splice_fd_in != 0
            || sqe.off != 0
        {
            return Err(errno::EINVAL);
        }
        let flags = sqe.op_flags;
        let known = TIMEOUT_ABS
            | TIMEOUT_BOOTTIME
            | TIMEOUT_REALTIME
            | TIMEOUT_ETIME_SUCCESS
            | TIMEOUT_IMMEDIATE_ARG;
        if flags & !known != 0 || (flags & (TIMEOUT_BOOTTIME | TIMEOUT_REALTIME)).count_ones() > 1 {
            return Err(errno::EINVAL);
        }
        if flags & TIMEOUT_REALTIME != 0 {
            return Err(errno::EOPNOTSUPP);
        }
        Ok(Op::LinkTimeout {
            nanoseconds: self.timeout_nanoseconds(sqe, flags)?,
            absolute: flags & TIMEOUT_ABS != 0,
        })
    }
}

fn accept(sqe: &Sqe) -> Result<Op, isize> {
    // Linux `io_accept_prep`：len/buf_index 必须为零，flags 只有 SOCK_CLOEXEC|SOCK_NONBLOCK。
    if sqe.len != 0 || sqe.buf_index != 0 || sqe.personality != 0 {
        return Err(errno::EINVAL);
    }
    let flags = sqe.op_flags as usize;
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 || sqe.ioprio & !ACCEPT_MULTISHOT != 0 {
        return Err(errno::EINVAL);
    }
    // multishot 需要“一个请求产生多个 CQE”，直接描述符需要注册文件表，均尚未实现。
    if sqe.ioprio & ACCEPT_MULTISHOT != 0 || sqe.splice_fd_in != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    Ok(Op::Accept {
        fd: fd_of(sqe),
        address: sqe.addr as usize,
        length: sqe.off as usize,
        flags,
    })
}

fn connect(sqe: &Sqe) -> Result<Op, isize> {
    // Linux `io_connect_prep`。
    if sqe.len != 0
        || sqe.buf_index != 0
        || sqe.op_flags != 0
        || sqe.splice_fd_in != 0
        || sqe.ioprio != 0
        || sqe.personality != 0
    {
        return Err(errno::EINVAL);
    }
    Ok(Op::Connect {
        fd: fd_of(sqe),
        address: sqe.addr as usize,
        length: sqe.off as usize,
    })
}

fn fsync(sqe: &Sqe) -> Result<Op, isize> {
    // Linux `io_fsync_prep`：addr/buf_index/splice_fd_in 必须为零，flags 只允许 DATASYNC。
    if sqe.ioprio != 0
        || sqe.personality != 0
        || sqe.addr != 0
        || sqe.buf_index != 0
        || sqe.splice_fd_in != 0
        || sqe.op_flags & !FSYNC_DATASYNC != 0
    {
        return Err(errno::EINVAL);
    }
    Ok(Op::Fsync { fd: fd_of(sqe) })
}

fn close(sqe: &Sqe) -> Result<Op, isize> {
    // Linux `io_close_prep`：除 fd 外的字段必须为零；固定文件引用在 prep 阶段即 EBADF。
    if sqe.ioprio != 0
        || sqe.personality != 0
        || sqe.off != 0
        || sqe.addr != 0
        || sqe.len != 0
        || sqe.op_flags != 0
        || sqe.buf_index != 0
    {
        return Err(errno::EINVAL);
    }
    if sqe.flags & IOSQE_FIXED_FILE != 0 {
        return Err(errno::EBADF);
    }
    // 直接描述符（file_index）需要注册文件表，尚未实现。
    if sqe.splice_fd_in != 0 {
        return Err(errno::EINVAL);
    }
    Ok(Op::Close { fd: fd_of(sqe) })
}

fn poll_add(sqe: &Sqe) -> Result<Op, isize> {
    // Linux `io_poll_add_prep`：off/addr/buf_index 必须为零；len 里只有 MULTI 一个合法位。
    if sqe.ioprio != 0
        || sqe.personality != 0
        || sqe.buf_index != 0
        || sqe.off != 0
        || sqe.addr != 0
        || sqe.len & !POLL_ADD_MULTI != 0
    {
        return Err(errno::EINVAL);
    }
    // multishot 需要“一个请求产生多个 CQE”（CQE_F_MORE）和重新布防，尚未实现。
    if sqe.len & POLL_ADD_MULTI != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    // ERR 与 HUP 总是关注（Linux `IO_POLL_UNMASK`）；高位的 EPOLLET/ONESHOT 等由本实现的一次性语义覆盖。
    let mask = (sqe.op_flags & 0xffff) as u16 as i16 | POLLERR | POLLHUP;
    Ok(Op::Poll {
        fd: fd_of(sqe),
        mask,
    })
}

/// `POLL_REMOVE`、`TIMEOUT_REMOVE` 与 `ASYNC_CANCEL`：目标是 worker 登记表里正在等待的成员。
fn cancel(sqe: &Sqe) -> Result<Op, isize> {
    let (target, all, by_fd) = match sqe.opcode {
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
                return Err(errno::EINVAL);
            }
            if sqe.op_flags & TIMEOUT_UPDATE_MASK != 0 {
                return Err(errno::EOPNOTSUPP);
            }
            if sqe.op_flags != 0 {
                return Err(errno::EINVAL);
            }
            (Target::UserData(sqe.addr, Only::Timeout), false, None)
        }
        OP_POLL_REMOVE => {
            // Linux `io_poll_remove_prep`。
            if sqe.ioprio != 0
                || sqe.personality != 0
                || sqe.buf_index != 0
                || sqe.splice_fd_in != 0
            {
                return Err(errno::EINVAL);
            }
            let flags = sqe.len;
            if flags & !(POLL_UPDATE_EVENTS | POLL_UPDATE_USER_DATA | POLL_ADD_MULTI) != 0
                || flags == POLL_ADD_MULTI
                || sqe.off != 0
                || sqe.op_flags != 0
            {
                return Err(errno::EINVAL);
            }
            if flags != 0 {
                return Err(errno::EOPNOTSUPP);
            }
            (Target::UserData(sqe.addr, Only::Poll), false, None)
        }
        OP_ASYNC_CANCEL => {
            // Linux `io_async_cancel_prep`。
            if sqe.ioprio != 0 || sqe.personality != 0 || sqe.off != 0 || sqe.splice_fd_in != 0 {
                return Err(errno::EINVAL);
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
                return Err(errno::EINVAL);
            }
            // Linux `io_cancel_req_match`：给了 FD/OP 条件就按它们匹配（USERDATA 可再叠加）；
            // 两者都没给时按 sqe->addr 的 user_data 匹配。
            let by_criteria = flags & (CANCEL_FD | CANCEL_OP) != 0;
            let target = if flags & CANCEL_ANY != 0 {
                Target::Any
            } else if by_criteria {
                Target::Match {
                    user_data: (flags & CANCEL_USERDATA != 0).then_some(sqe.addr),
                    opcode: (flags & CANCEL_OP != 0).then_some(sqe.len as u8),
                    file: None,
                }
            } else {
                Target::UserData(sqe.addr, Only::Any)
            };
            // 没有注册文件表：fixed fd 引用与负 fd 一样在 issue 时得到 -EBADF。
            let by_fd = (flags & CANCEL_FD != 0).then(|| {
                (flags & CANCEL_FD_FIXED == 0)
                    .then(|| usize::try_from(sqe.fd).ok())
                    .flatten()
            });
            (target, flags & CANCEL_ALL != 0, by_fd)
        }
        _ => unreachable!("cancel handles exactly the three cancel-style opcodes"),
    };
    Ok(Op::Cancel { target, all, by_fd })
}
