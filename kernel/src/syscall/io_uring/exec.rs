//! 请求的执行：prep 后拥有全部用户数据的 [`Op`]、它的 issue，以及会等待的 [`Wait`] 的 `async fn`。
//!
//! 所有 opcode 的执行都显式接收提交线程，不使用 `current_task()`，因此同一份代码既能在提交者上下文里
//! 运行，也能在 worker 线程里继续（链的后继成员）。文件描述符在 issue 时才解析而不是 prep 时：链里前一个
//! 成员（如 `close`）改变 fd 表后，后继看到的是新状态，与 Linux `io_assign_file` 一致。
//!
//! issue 的结果分两种：同步得出的 [`Outcome`]（普通文件、显式偏移、fsync、close、cancel…，不分配 future），
//! 或需要等待就绪的 [`Wait`]（管道/socket 的流式读写、poll、timeout），后者由 [`drive`] 驱动。

use alloc::{boxed::Box, sync::Arc, sync::Weak, vec::Vec};
use syscall_abi::errno;

use super::{
    lane,
    wait::{Kind, Target, Waiter, event_count_reached},
    worker,
};
use crate::{
    file::{OpenFileDescription, OpenFileKind},
    fs::{O_NONBLOCK, chardev::UringCommand},
    io_uring::Ring,
    socket::{SocketCapacityWait, SocketError},
    syscall::{
        fs::{
            AsyncPositionedRead, AsyncReadStep, begin_current_position_read, begin_positioned_read,
            check_readable, check_writable, open_at, perform_read, perform_write, positioned_read,
            positioned_write, statx_at, statx_request_valid, sync_ofd,
        },
        socket::{
            ConnectBegin, MSG_NOSIGNAL, accept_once, connect_begin, socket_error, socket_of,
            write_vectors,
        },
        user_iovec::UserIoVec,
    },
    task::TaskControlBlock,
};

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

/// 一个成员的完成：CQE 的 `res` 与它是否“失败”。失败使链断开（`IOSQE_IO_LINK`），
/// 并使 `IOSQE_CQE_SKIP_SUCCESS` 失效；它不等同于 `res < 0`（短读写失败，`ETIME_SUCCESS` 的超时不失败）。
pub(super) struct Outcome {
    pub(super) res: i32,
    pub(super) fail: bool,
}

impl Outcome {
    pub(super) fn of(res: isize) -> Self {
        Self {
            res: res as i32,
            fail: res < 0,
        }
    }

    pub(super) fn canceled() -> Self {
        Self::of(-errno::ECANCELED)
    }

    /// 读写结果：传输量不等于请求量即失败（Linux 以 `res != req->cqe.res` 判定）。
    fn transfer(res: isize, total: usize) -> Self {
        Self {
            res: res as i32,
            fail: res < 0 || res as usize != total,
        }
    }
}

/// 一个已通过 prep 校验、拥有全部用户数据的请求。`fd` 为 `None` 表示 prep 时就已无效（负数或 fixed 引用）。
pub(super) enum Op {
    DeviceCommand {
        fd: Option<usize>,
        command: UringCommand,
    },
    ReadWrite {
        fd: Option<usize>,
        vectors: Vec<UserIoVec>,
        total: usize,
        write: bool,
        vectored: bool,
        /// `sqe->off`：`-1` 使用并推进当前 position，其他负值非法。
        offset: i64,
    },
    Fsync {
        fd: Option<usize>,
    },
    /// `IORING_OP_OPENAT`：在阻塞车道上以提交线程的 fd 表、cwd 与凭据打开。
    OpenAt {
        dirfd: isize,
        path: Vec<u8>,
        flags: u32,
        mode: u32,
    },
    /// `IORING_OP_STATX`：在阻塞车道上以提交线程的 cwd、凭据与地址空间执行（解析路径可能读盘）。
    Statx {
        dirfd: isize,
        path: Vec<u8>,
        flags: u32,
        mask: u32,
        buffer: usize,
    },
    Close {
        fd: Option<usize>,
    },
    Poll {
        fd: Option<usize>,
        mask: i16,
    },
    /// `IORING_OP_SEND`/`IORING_OP_RECV`：只对 socket，`MSG_NOSIGNAL` 恒开（SIGPIPE 不发给提交线程）。
    Socket {
        fd: Option<usize>,
        vectors: Vec<UserIoVec>,
        total: usize,
        write: bool,
        /// `MSG_DONTWAIT`：会阻塞时直接返回 `-EAGAIN`。
        dontwait: bool,
    },
    Accept {
        fd: Option<usize>,
        address: usize,
        length: usize,
        flags: usize,
    },
    Connect {
        fd: Option<usize>,
        address: usize,
        length: usize,
    },
    Timeout {
        nanoseconds: u64,
        absolute: bool,
        /// 非零：除 timeout 外又发布这么多 CQE 时完成。
        count: u32,
        etime_success: bool,
    },
    /// `IORING_OP_LINK_TIMEOUT`：约束它前面那个链成员。前一个成员需要等待时由链执行者摘下它装成定时器
    /// （[`Op::link_deadline`]）；走到 issue 说明前一个成员已经同步完成，定时器没有对象，以 `-ECANCELED` 结束。
    LinkTimeout {
        nanoseconds: u64,
        absolute: bool,
    },
    Cancel {
        target: Target,
        /// `IORING_ASYNC_CANCEL_ALL`：取消全部匹配项并返回个数，否则只取消第一个。
        all: bool,
        /// `IORING_ASYNC_CANCEL_FD`：按这个 fd 所指的 OFD 匹配；外层 `Some` 表示给了 FD 条件，内层 `None` 是无效 fd（fixed 或负数）。
        by_fd: Option<Option<usize>>,
    },
}

/// issue 的结果。
#[allow(
    clippy::large_enum_variant,
    reason = "Issue is a transient return value moved once into the chain future; boxing Wait would add an allocation per waiting request"
)]
pub(super) enum Issue {
    Done(Outcome),
    Wait(Wait),
}

/// 可能等待就绪的 op，已解析出目标 OFD。
#[allow(
    clippy::large_enum_variant,
    reason = "each Wait lives inside one boxed chain future; boxing a variant would add a second allocation per request"
)]
pub(super) enum Wait {
    /// 设备 owner 持有协议状态；reactor 只检查 ready，用户页复制经阻塞车道执行。
    DeviceCommand {
        ofd: Arc<OpenFileDescription>,
        command: UringCommand,
    },
    /// pipe/socket/eventfd 等流式读写：`EAGAIN` 时等目标 OFD 就绪再试。
    Stream {
        ofd: Arc<OpenFileDescription>,
        vectors: Vec<UserIoVec>,
        total: usize,
        write: bool,
        /// 经 socket 发送并强制 `MSG_NOSIGNAL`（`IORING_OP_SEND`）。
        no_signal: bool,
        /// 会阻塞时返回 `-EAGAIN` 而不是等待（`MSG_DONTWAIT`）。
        dontwait: bool,
    },
    Accept {
        ofd: Arc<OpenFileDescription>,
        flags: usize,
        address: usize,
        length: usize,
    },
    /// 握手已发起：等 socket 可写或出错，再读连接结果。
    Connect { ofd: Arc<OpenFileDescription> },
    /// Unix 目标 backlog 已满：等它恢复容量后重新发起 connect（Linux 在 io-wq 里等同一个容量）。
    ConnectBacklog {
        ofd: Arc<OpenFileDescription>,
        address: usize,
        length: usize,
        capacity: Arc<SocketCapacityWait>,
    },
    /// regular-file 读（显式偏移或当前位置）：cache 未命中时开始异步填页并让出，不占用任何线程。
    FileRead {
        read: AsyncPositionedRead,
        vectors: Vec<UserIoVec>,
        total: usize,
    },
    /// 会睡眠在磁盘上的作业（regular 文件写、fsync、当前位置读、无法不阻塞的读），交给阻塞车道。
    Lane {
        work: lane::Work,
        /// 作业操作的 OFD，仅供按 fd 取消匹配（作业本身已捕获它）；openat 之类没有目标 OFD。
        file: Option<Arc<OpenFileDescription>>,
        /// 读写作业传输量不等于请求量即失败；fsync 为 `None`。
        total: Option<usize>,
    },
    /// 一次性 poll：目标 OFD 出现 `mask` 中任一事件时以 revents 完成。
    Poll {
        ofd: Arc<OpenFileDescription>,
        mask: i16,
    },
    /// 绝对 monotonic deadline，或“除 timeout 外又发布 N 个 CQE”（`target_events`）。
    Timeout {
        deadline: u64,
        target_events: Option<u32>,
        etime_success: bool,
    },
}

impl Wait {
    /// 这个等待所操作的 OFD（按 fd 取消据此匹配）；timeout 没有。
    pub(super) fn file(&self) -> Option<Arc<OpenFileDescription>> {
        match self {
            Wait::Stream { ofd, .. }
            | Wait::DeviceCommand { ofd, .. }
            | Wait::Accept { ofd, .. }
            | Wait::Connect { ofd }
            | Wait::ConnectBacklog { ofd, .. }
            | Wait::Poll { ofd, .. } => Some(ofd.clone()),
            Wait::FileRead { read, .. } => Some(read.ofd().clone()),
            Wait::Lane { file, .. } => file.clone(),
            Wait::Timeout { .. } => None,
        }
    }

    pub(super) fn kind(&self) -> Kind {
        match self {
            Wait::Stream { .. }
            | Wait::DeviceCommand { .. }
            | Wait::Accept { .. }
            | Wait::Connect { .. }
            | Wait::ConnectBacklog { .. }
            | Wait::FileRead { .. }
            | Wait::Lane { .. } => Kind::Io,
            Wait::Poll { .. } => Kind::Poll,
            Wait::Timeout { .. } => Kind::Timeout,
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

fn ofd_of(task: &TaskControlBlock, fd: Option<usize>) -> Option<Arc<OpenFileDescription>> {
    fd.and_then(|fd| task.fd_get(fd))
}

impl Op {
    /// `LinkTimeout` 在 `now` 时刻装成定时器的绝对 monotonic deadline；其他 op 返回 `None`。
    pub(super) fn link_deadline(&self, now: u64) -> Option<u64> {
        match *self {
            Op::LinkTimeout {
                nanoseconds,
                absolute,
            } => Some(if absolute {
                nanoseconds
            } else {
                now.saturating_add(nanoseconds)
            }),
            _ => None,
        }
    }

    /// 在 `task` 的上下文里 issue：同步 op 直接得出结果，会等待的 op 返回 [`Wait`]。
    pub(super) fn issue(self, task: &TaskControlBlock, ring: &Arc<Ring>) -> Issue {
        match self {
            Op::DeviceCommand { fd, command } => {
                let Some(ofd) = ofd_of(task, fd) else {
                    return Issue::Done(Outcome::of(-errno::EBADF));
                };
                let OpenFileKind::Device(_) = &ofd.kind else {
                    return Issue::Done(Outcome::of(-errno::EOPNOTSUPP));
                };
                Issue::Wait(Wait::DeviceCommand { ofd, command })
            }
            Op::ReadWrite {
                fd,
                vectors,
                total,
                write,
                vectored,
                offset,
            } => issue_read_write(task, fd, vectors, total, write, vectored, offset),
            Op::Fsync { fd } => match ofd_of(task, fd) {
                None => Issue::Done(Outcome::of(-errno::EBADF)),
                Some(ofd) => lane_issue(Some(ofd.clone()), move |_| sync_ofd(&ofd), None),
            },
            Op::OpenAt {
                dirfd,
                path,
                flags,
                mode,
            } => lane_issue(
                None,
                move |task| {
                    let result = open_at(task, dirfd, path, flags, mode);
                    // 车道线程没有信号重启语义：被打断的阻塞 open（如 FIFO）以 EINTR 完成。
                    if result == crate::syscall::INTERNAL_RESTART_SYS {
                        -errno::EINTR
                    } else {
                        result
                    }
                },
                None,
            ),
            Op::Statx {
                dirfd,
                path,
                flags,
                mask,
                buffer,
            } => {
                if !statx_request_valid(flags, mask) {
                    return Issue::Done(Outcome::of(-errno::EINVAL));
                }
                lane_issue(
                    None,
                    move |task| statx_at(task, dirfd, path, flags, buffer),
                    None,
                )
            }
            Op::Close { fd } => Issue::Done(Outcome::of(fd.map_or(-errno::EBADF, |fd| {
                task.fd_close(fd).map_or(-errno::EBADF, |flushed| {
                    flushed.map_or_else(crate::syscall::fs::ferr, |()| 0)
                })
            }))),
            Op::Poll { fd, mask } => match ofd_of(task, fd) {
                Some(ofd) => Issue::Wait(Wait::Poll { ofd, mask }),
                None => Issue::Done(Outcome::of(-errno::EBADF)),
            },
            Op::Socket {
                fd,
                vectors,
                total,
                write,
                dontwait,
            } => match ofd_of(task, fd) {
                None => Issue::Done(Outcome::of(-errno::EBADF)),
                Some(ofd) => match socket_of(&ofd) {
                    Err(error) => Issue::Done(Outcome::of(error)),
                    Ok(_) => Issue::Wait(Wait::Stream {
                        ofd,
                        vectors,
                        total,
                        write,
                        no_signal: write,
                        dontwait,
                    }),
                },
            },
            Op::Accept {
                fd,
                address,
                length,
                flags,
            } => match ofd_of(task, fd) {
                None => Issue::Done(Outcome::of(-errno::EBADF)),
                Some(ofd) => match socket_of(&ofd) {
                    Err(error) => Issue::Done(Outcome::of(error)),
                    Ok(_) => Issue::Wait(Wait::Accept {
                        ofd,
                        flags,
                        address,
                        length,
                    }),
                },
            },
            Op::Connect {
                fd,
                address,
                length,
            } => issue_connect(task, fd, address, length),
            Op::Timeout {
                nanoseconds,
                absolute,
                count,
                etime_success,
            } => Issue::Wait(Wait::Timeout {
                deadline: if absolute {
                    nanoseconds
                } else {
                    crate::timer::get_time_ns().saturating_add(nanoseconds)
                },
                // 目标序号在 issue 时取（Linux `io_timeout`），而不是 prep 时。
                target_events: (count != 0).then(|| ring.events_posted().wrapping_add(count)),
                etime_success,
            }),
            Op::LinkTimeout { .. } => Issue::Done(Outcome {
                res: -(errno::ECANCELED as i32),
                // 没有对象可约束不是链失败：Linux 的 link timeout 取消不使其后成员被取消。
                fail: false,
            }),
            Op::Cancel {
                mut target,
                all,
                by_fd,
            } => {
                if let Some(fd) = by_fd {
                    // fd 在 issue 时解析：无效 fd 是 -EBADF（Linux `io_async_cancel` 的 `io_file_get_normal` 失败）。
                    let Some(ofd) = ofd_of(task, fd) else {
                        return Issue::Done(Outcome::of(-errno::EBADF));
                    };
                    if let Target::Match { file, .. } = &mut target {
                        *file = Some(ofd);
                    }
                }
                Issue::Done(Outcome::of(worker::cancel(ring, &target, all) as isize))
            }
        }
    }
}

fn issue_read_write(
    task: &TaskControlBlock,
    fd: Option<usize>,
    vectors: Vec<UserIoVec>,
    total: usize,
    write: bool,
    vectored: bool,
    offset: i64,
) -> Issue {
    let done = |res: isize| Issue::Done(Outcome::transfer(res, total));
    // 持久文件（OpenFileKind::Inode）的读写可能睡眠在磁盘上，不能在提交线程或 reactor 上执行。
    let disk_backed = |ofd: &OpenFileDescription| matches!(ofd.kind, OpenFileKind::Inode);
    let Some(ofd) = ofd_of(task, fd) else {
        return Issue::Done(Outcome::of(-errno::EBADF));
    };
    let access = if write {
        check_writable(&ofd)
    } else {
        check_readable(&ofd)
    };
    if let Err(error) = access {
        return Issue::Done(Outcome::of(error));
    }
    if offset < -1 {
        return Issue::Done(Outcome::of(-errno::EINVAL));
    }
    if offset >= 0 && positionable(&ofd) {
        // 显式偏移：pread/pwrite 语义，不触碰共享 OFD position。
        if disk_backed(&ofd) {
            if !write {
                match begin_positioned_read(&ofd, &vectors, offset) {
                    Ok(Some(read)) => {
                        return Issue::Wait(Wait::FileRead {
                            read,
                            vectors,
                            total,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => return Issue::Done(Outcome::of(error)),
                }
            }
            return lane_issue(
                Some(ofd.clone()),
                move |task| {
                    if write {
                        positioned_write(task, &ofd, &vectors, offset, None)
                    } else {
                        positioned_read(task, &ofd, &vectors, offset)
                    }
                },
                Some(total),
            );
        }
        return done(if write {
            positioned_write(task, &ofd, &vectors, offset, None)
        } else {
            positioned_read(task, &ofd, &vectors, offset)
        });
    }
    if total == 0 && vectored {
        // 与 readv/writev 一致：空 iovec 总长不观察文件。
        if write {
            task.account_write_result(0);
        } else {
            task.account_read_result(0);
        }
        return done(0);
    }
    if can_block(&ofd) {
        return Issue::Wait(Wait::Stream {
            ofd,
            vectors,
            total,
            write,
            no_signal: false,
            dontwait: false,
        });
    }
    if disk_backed(&ofd) {
        if !write {
            match begin_current_position_read(&ofd, &vectors) {
                Ok(Some(read)) => {
                    return Issue::Wait(Wait::FileRead {
                        read,
                        vectors,
                        total,
                    });
                }
                Ok(None) => {}
                Err(error) => return Issue::Done(Outcome::of(error)),
            }
        }
        return lane_issue(
            Some(ofd.clone()),
            move |task| {
                if write {
                    perform_write(task, &ofd, &vectors, total, true)
                } else {
                    perform_read(task, &ofd, &vectors, total, true)
                }
            },
            Some(total),
        );
    }
    done(if write {
        perform_write(task, &ofd, &vectors, total, true)
    } else {
        perform_read(task, &ofd, &vectors, total, true)
    })
}

/// 把会睡眠的作业包成 [`Wait::Lane`]。
fn lane_issue(
    file: Option<Arc<OpenFileDescription>>,
    work: impl FnOnce(&TaskControlBlock) -> isize + Send + 'static,
    total: Option<usize>,
) -> Issue {
    match Box::try_new(work) {
        Ok(work) => {
            let work: lane::Work = work;
            Issue::Wait(Wait::Lane { work, file, total })
        }
        Err(_) => Issue::Done(Outcome::of(-errno::ENOMEM)),
    }
}

fn lane_outcome(result: isize, total: Option<usize>) -> Outcome {
    match total {
        Some(total) => Outcome::transfer(result, total),
        None => Outcome::of(result),
    }
}

/// future 直接被丢弃时也拆掉 Waiter → Ticket → notify(Waiter) 引用环。
/// 票据只在车道作业实际执行期间拥有目标 OFD，不能让一次 ring close 永久保活连接。
struct LaneWait<'a>(&'a Waiter);

impl Drop for LaneWait<'_> {
    fn drop(&mut self) {
        self.0.detach_lane();
    }
}

/// 在阻塞车道上执行作业并等它完成。
pub(super) async fn run_lane(
    work: lane::Work,
    total: Option<usize>,
    task: &Weak<TaskControlBlock>,
    waiter: &Waiter,
) -> Outcome {
    let Ok(ticket) = lane::submit(work, task.clone(), waiter.notifier()) else {
        return Outcome::of(-errno::ENOMEM);
    };
    waiter.attach_lane(ticket.clone());
    let _attachment = LaneWait(waiter);
    loop {
        // 只有尚未开始的作业才能真正取消；已开始的作业副作用会发生，等它完成并报告真实结果。
        if waiter.take_canceled() && ticket.try_cancel() {
            return Outcome::canceled();
        }
        // 先清通知再取结果：作业在两者之间完成会重新置位，`block` 立即被重新 poll。
        waiter.clear_notified();
        if let Some(result) = ticket.take() {
            return lane_outcome(result, total);
        }
        waiter.block().await;
    }
}

/// connect 先发起一次：已连接或出错同步完成；握手进行中或 Unix 目标 backlog 已满（阻塞 socket）才等待，
/// `O_NONBLOCK` socket 的 backlog 已满以 `-EAGAIN` 完成。
fn issue_connect(
    task: &TaskControlBlock,
    fd: Option<usize>,
    address: usize,
    length: usize,
) -> Issue {
    let Some(ofd) = ofd_of(task, fd) else {
        return Issue::Done(Outcome::of(-errno::EBADF));
    };
    let client = match socket_of(&ofd) {
        Ok(client) => client,
        Err(error) => return Issue::Done(Outcome::of(error)),
    };
    match connect_begin(task, &client, address, length) {
        Ok(ConnectBegin::Connected) => Issue::Done(Outcome::of(0)),
        Ok(ConnectBegin::BacklogFull(_)) if ofd.status_flags() & O_NONBLOCK != 0 => {
            Issue::Done(Outcome::of(-errno::EAGAIN))
        }
        Ok(ConnectBegin::BacklogFull(capacity)) => match Arc::try_new(capacity) {
            Ok(capacity) => Issue::Wait(Wait::ConnectBacklog {
                ofd,
                address,
                length,
                capacity,
            }),
            Err(_) => Issue::Done(Outcome::of(-errno::ENOMEM)),
        },
        Ok(ConnectBegin::InProgress) if ofd.status_flags() & O_NONBLOCK != 0 => {
            Issue::Done(Outcome::of(-errno::EINPROGRESS))
        }
        Ok(ConnectBegin::InProgress) => Issue::Wait(Wait::Connect { ofd }),
        Err(error) => Issue::Done(Outcome::of(error)),
    }
}

/// 握手已发起：等 socket 可写或出错，再读连接结果。
async fn await_connection(
    ofd: &Arc<OpenFileDescription>,
    task: &Weak<TaskControlBlock>,
    waiter: &Waiter,
) -> Outcome {
    loop {
        if waiter.take_canceled() {
            return Outcome::canceled();
        }
        let result = match socket_of(ofd).map(|client| client.connection_result()) {
            Ok(Ok(())) => 0,
            Ok(Err(SocketError::InProgress)) => {
                waiter.ofd_ready(ofd, POLLOUT | POLLERR, task).await;
                continue;
            }
            Ok(Err(error)) => socket_error(error),
            Err(error) => error,
        };
        return Outcome::of(result);
    }
}

/// 驱动一个会等待的 op 直到完成；等待点把条件登记到 `waiter`，由 worker 在条件满足后重新 poll。
///
/// 提交线程只在每次重试的一小段持有强引用、不跨 `.await`：强引用会延长其 TCB 寿命，
/// 升级失败（线程已退出）以 `-ECANCELED` 结束。
pub(super) async fn drive(
    op: Wait,
    ring: Arc<Ring>,
    task: Weak<TaskControlBlock>,
    waiter: Waiter,
) -> Outcome {
    match op {
        Wait::DeviceCommand { ofd, command } => {
            super::device::drive(ofd, command, &task, &waiter).await
        }
        Wait::Stream {
            ofd,
            vectors,
            total,
            write,
            no_signal,
            dontwait,
        } => {
            // ERR/HUP 一并关注，使对端关闭也能唤醒并让重试读到 EOF/EPIPE。
            let mask = (if write { POLLOUT } else { POLLIN }) | POLLERR | POLLHUP;
            loop {
                if waiter.take_canceled() {
                    return Outcome::canceled();
                }
                let Some(strong) = task.upgrade() else {
                    return Outcome::canceled();
                };
                let result = match (&ofd.kind, write && no_signal) {
                    (OpenFileKind::Socket(socket), true) => {
                        write_vectors(&strong, &ofd, socket, &vectors, total, true, MSG_NOSIGNAL)
                    }
                    _ if write => perform_write(&strong, &ofd, &vectors, total, true),
                    _ => perform_read(&strong, &ofd, &vectors, total, true),
                };
                drop(strong);
                // 用户自己设置了 O_NONBLOCK（或本次 MSG_DONTWAIT）时如实返回 EAGAIN，不挂起。
                if result != -errno::EAGAIN || dontwait || ofd.status_flags() & O_NONBLOCK != 0 {
                    return Outcome::transfer(result, total);
                }
                waiter.ofd_ready(&ofd, mask, &task).await;
            }
        }
        Wait::FileRead {
            mut read,
            vectors,
            total,
        } => {
            let notify = waiter.notifier();
            loop {
                if waiter.take_canceled() {
                    return Outcome::canceled();
                }
                let Some(strong) = task.upgrade() else {
                    return Outcome::canceled();
                };
                waiter.clear_notified();
                let step = read.step(&strong, &vectors, &notify);
                drop(strong);
                match step {
                    AsyncReadStep::Done(result) => {
                        if read.publish_position(result) {
                            return Outcome::transfer(result, total);
                        }
                        // position 正被同步读写占用：reactor 不能等它，交给车道发布。
                        let work: lane::Work = match Box::try_new(move |_: &TaskControlBlock| {
                            read.publish_position_blocking(result);
                            result
                        }) {
                            Ok(work) => work,
                            Err(_) => return Outcome::of(-errno::ENOMEM),
                        };
                        return run_lane(work, Some(total), &task, &waiter).await;
                    }
                    AsyncReadStep::Wait => waiter.block().await,
                    AsyncReadStep::Fallback => {
                        let work: lane::Work = match Box::try_new(move |task: &TaskControlBlock| {
                            read.finish_blocking(task, &vectors)
                        }) {
                            Ok(work) => work,
                            Err(_) => return Outcome::of(-errno::ENOMEM),
                        };
                        return run_lane(work, Some(total), &task, &waiter).await;
                    }
                }
            }
        }
        Wait::Lane { work, total, .. } => run_lane(work, total, &task, &waiter).await,
        Wait::Accept {
            ofd,
            flags,
            address,
            length,
        } => loop {
            if waiter.take_canceled() {
                return Outcome::canceled();
            }
            let Some(strong) = task.upgrade() else {
                return Outcome::canceled();
            };
            let result = match socket_of(&ofd) {
                Ok(listener) => accept_once(&strong, &listener, flags, address, length),
                Err(error) => error,
            };
            drop(strong);
            if result != -errno::EAGAIN || ofd.status_flags() & O_NONBLOCK != 0 {
                return Outcome::of(result);
            }
            waiter
                .ofd_ready(&ofd, POLLIN | POLLERR | POLLHUP, &task)
                .await;
        },
        Wait::Connect { ofd } => await_connection(&ofd, &task, &waiter).await,
        Wait::ConnectBacklog {
            ofd,
            address,
            length,
            mut capacity,
        } => loop {
            waiter.capacity(&capacity).await;
            if waiter.take_canceled() {
                return Outcome::canceled();
            }
            let Some(strong) = task.upgrade() else {
                return Outcome::canceled();
            };
            let begun = match socket_of(&ofd) {
                Ok(client) => connect_begin(&strong, &client, address, length),
                Err(error) => Err(error),
            };
            drop(strong);
            match begun {
                Ok(ConnectBegin::Connected) => return Outcome::of(0),
                Ok(ConnectBegin::InProgress) => {
                    return await_connection(&ofd, &task, &waiter).await;
                }
                // 容量又被别的 connect 抢走：等下一次。
                Ok(ConnectBegin::BacklogFull(next)) => match Arc::try_new(next) {
                    Ok(next) => capacity = next,
                    Err(_) => return Outcome::of(-errno::ENOMEM),
                },
                Err(error) => return Outcome::of(error),
            }
        },
        Wait::Poll { ofd, mask } => loop {
            if waiter.take_canceled() {
                return Outcome::canceled();
            }
            let Some(strong) = task.upgrade() else {
                return Outcome::canceled();
            };
            let ready = ofd.poll_events(mask, strong.as_ref());
            drop(strong);
            if ready != 0 {
                return Outcome::of(isize::from(ready));
            }
            waiter.ofd_ready(&ofd, mask, &task).await;
        },
        Wait::Timeout {
            deadline,
            target_events,
            etime_success,
        } => loop {
            if waiter.take_canceled() {
                return Outcome::canceled();
            }
            // 先看计数：满足计数以 0 完成；否则只可能是 deadline 到期。
            if target_events.is_some_and(|target| event_count_reached(&ring, target)) {
                return Outcome::of(0);
            }
            if deadline <= crate::timer::get_time_ns() {
                // ETIME_SUCCESS 只让链不断开，CQE 的 res 仍是 -ETIME（Linux `io_timeout_fn`）。
                return Outcome {
                    res: -(errno::ETIME as i32),
                    fail: !etime_success,
                };
            }
            waiter.timer(deadline, target_events, &ring).await;
        },
    }
}
