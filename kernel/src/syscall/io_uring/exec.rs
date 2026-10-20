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
    fs::O_NONBLOCK,
    io_uring::Ring,
    socket::SocketError,
    syscall::{
        fs::{
            AsyncPositionedRead, AsyncReadStep, begin_current_position_read, begin_positioned_read,
            check_readable, check_writable, perform_read, perform_write, positioned_read,
            positioned_write, sync_ofd,
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
    Cancel {
        target: Target,
        /// `IORING_ASYNC_CANCEL_ALL`：取消全部匹配项并返回个数，否则只取消第一个。
        all: bool,
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
    /// regular-file 读（显式偏移或当前位置）：cache 未命中时开始异步填页并让出，不占用任何线程。
    FileRead {
        read: AsyncPositionedRead,
        vectors: Vec<UserIoVec>,
        total: usize,
    },
    /// 会睡眠在磁盘上的作业（regular 文件写、fsync、当前位置读、无法不阻塞的读），交给阻塞车道。
    Lane {
        work: lane::Work,
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
    pub(super) fn kind(&self) -> Kind {
        match self {
            Wait::Stream { .. }
            | Wait::Accept { .. }
            | Wait::Connect { .. }
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
    /// 在 `task` 的上下文里 issue：同步 op 直接得出结果，会等待的 op 返回 [`Wait`]。
    pub(super) fn issue(self, task: &TaskControlBlock, ring: &Arc<Ring>) -> Issue {
        match self {
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
                Some(ofd) => lane_issue(move |_| sync_ofd(&ofd), None),
            },
            Op::Close { fd } => Issue::Done(Outcome::of(fd.map_or(-errno::EBADF, |fd| {
                task.fd_close(fd).map_or(-errno::EBADF, |_| 0)
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
            Op::Cancel { target, all } => {
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
    work: impl FnOnce(&TaskControlBlock) -> isize + Send + 'static,
    total: Option<usize>,
) -> Issue {
    match Box::try_new(work) {
        Ok(work) => {
            let work: lane::Work = work;
            Issue::Wait(Wait::Lane { work, total })
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

/// 在阻塞车道上执行作业并等它完成。
async fn run_lane(
    work: lane::Work,
    total: Option<usize>,
    task: &Weak<TaskControlBlock>,
    waiter: &Waiter,
) -> Outcome {
    let Ok(ticket) = lane::submit(work, task.clone(), waiter.notifier()) else {
        return Outcome::of(-errno::ENOMEM);
    };
    loop {
        if waiter.take_canceled() {
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

/// connect 只发起一次：已连接或出错同步完成，握手进行中才等待；Unix 目标 backlog 已满以
/// `-EAGAIN` 完成（已知偏差：Linux 会在 io-wq 里等容量）。
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
        Ok(ConnectBegin::BacklogFull(_)) => Issue::Done(Outcome::of(-errno::EAGAIN)),
        Ok(ConnectBegin::InProgress) if ofd.status_flags() & O_NONBLOCK != 0 => {
            Issue::Done(Outcome::of(-errno::EINPROGRESS))
        }
        Ok(ConnectBegin::InProgress) => Issue::Wait(Wait::Connect { ofd }),
        Err(error) => Issue::Done(Outcome::of(error)),
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
        Wait::Lane { work, total } => run_lane(work, total, &task, &waiter).await,
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
        Wait::Connect { ofd } => loop {
            if waiter.take_canceled() {
                return Outcome::canceled();
            }
            let result = match socket_of(&ofd).map(|client| client.connection_result()) {
                Ok(Ok(())) => 0,
                Ok(Err(SocketError::InProgress)) => {
                    waiter.ofd_ready(&ofd, POLLOUT | POLLERR, &task).await;
                    continue;
                }
                Ok(Err(error)) => socket_error(error),
                Err(error) => error,
            };
            return Outcome::of(result);
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
