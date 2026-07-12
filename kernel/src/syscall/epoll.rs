use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{Epoll, EpollChange, EpollEvent, OpenFileDescription, OpenFileKind},
    task::{WaitResult, current_task, wait_for_poll},
};

use super::{errno, poll::ofd_wait_keys};

const EPOLL_CLOEXEC: usize = 0x80000;
const EPOLL_CTL_ADD: usize = 1;
const EPOLL_CTL_DEL: usize = 2;
const EPOLL_CTL_MOD: usize = 3;
const EPOLLET: u32 = 1 << 31;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLL_ALLOWED: u32 = 0x001 | 0x004 | 0x008 | 0x010 | EPOLLET | EPOLLONESHOT;

struct ReadyEvent {
    fd: usize,
    flags: u32,
    data: u64,
    edge: bool,
    oneshot: bool,
}

struct Evaluation {
    ready: Vec<ReadyEvent>,
    keys: Vec<crate::task::PollWaitKey>,
}

fn epoll_fd(fd: usize) -> Result<Arc<Epoll>, isize> {
    let ofd = current_task().unwrap().fd_get(fd).ok_or(-errno::EBADF)?;
    match &ofd.kind {
        OpenFileKind::Epoll(epoll) => Ok(epoll.clone()),
        _ => Err(-errno::EINVAL),
    }
}

fn read_event(pointer: usize) -> Result<EpollEvent, isize> {
    if pointer == 0 {
        return Err(-errno::EFAULT);
    }
    let mut bytes = [0u8; 16];
    current_task()
        .unwrap()
        .copy_from_user(pointer, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    let event = EpollEvent {
        events: u32::from_ne_bytes(bytes[..4].try_into().unwrap()),
        data: u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
    };
    if event.events & !EPOLL_ALLOWED != 0 {
        return Err(-errno::EINVAL);
    }
    Ok(event)
}

pub(crate) fn sys_epoll_create1(flags: usize) -> isize {
    if flags & !EPOLL_CLOEXEC != 0 {
        return -errno::EINVAL;
    }
    current_task()
        .unwrap()
        .fd_allocate(
            OpenFileDescription::epoll(Epoll::new()),
            flags & EPOLL_CLOEXEC != 0,
        )
        .map_or(-errno::EMFILE, |fd| fd as isize)
}

pub(crate) fn sys_epoll_ctl(
    epoll_fd_number: usize,
    operation: usize,
    fd: usize,
    event: usize,
) -> isize {
    if epoll_fd_number == fd {
        return -errno::EINVAL;
    }
    let epoll = match epoll_fd(epoll_fd_number) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let (change, watched, event) = match operation {
        EPOLL_CTL_ADD => (
            EpollChange::Add,
            current_task().unwrap().fd_get(fd),
            match read_event(event) {
                Ok(value) => Some(value),
                Err(error) => return error,
            },
        ),
        EPOLL_CTL_DEL => (EpollChange::Delete, None, None),
        EPOLL_CTL_MOD => (
            EpollChange::Modify,
            None,
            match read_event(event) {
                Ok(value) => Some(value),
                Err(error) => return error,
            },
        ),
        _ => return -errno::EINVAL,
    };
    if operation == EPOLL_CTL_ADD && watched.is_none() {
        return -errno::EBADF;
    }
    epoll.change(change, fd, watched, event).map_or_else(
        |error| -match error {
            crate::fs::EpollChangeError::Exists => errno::EEXIST,
            crate::fs::EpollChangeError::NotFound => errno::ENOENT,
            crate::fs::EpollChangeError::Invalid => errno::EINVAL,
        },
        |()| 0,
    )
}

fn evaluate(epoll: &Arc<Epoll>, maximum: usize) -> Result<Evaluation, isize> {
    let snapshot = epoll.snapshot().map_err(|_| -errno::ENOMEM)?;
    let mut ready = Vec::new();
    let mut keys = Vec::new();
    let mut observed = Vec::new();
    ready
        .try_reserve_exact(maximum.min(snapshot.len()))
        .map_err(|_| -errno::ENOMEM)?;
    for interest in snapshot {
        let current = interest.ofd.poll_events(interest.event.events as i16) as u32;
        observed.push((interest.fd, current));
        keys.extend(ofd_wait_keys(&interest.ofd));
        if interest.disabled || current == 0 {
            continue;
        }
        let edge = interest.event.events & EPOLLET != 0;
        if edge && current & !interest.last_ready == 0 {
            continue;
        }
        ready.push(ReadyEvent {
            fd: interest.fd,
            flags: current,
            data: interest.event.data,
            edge,
            oneshot: interest.event.events & EPOLLONESHOT != 0,
        });
        if ready.len() == maximum {
            break;
        }
    }
    epoll.clear_absent_edges(&observed);
    Ok(Evaluation { ready, keys })
}

pub(crate) fn sys_epoll_pwait(
    epoll_fd_number: usize,
    events: usize,
    maximum: usize,
    timeout_ms: isize,
    signal_mask: usize,
    signal_set_size: usize,
) -> isize {
    if events == 0 || maximum == 0 || maximum > 1024 || timeout_ms < -1 {
        return -errno::EINVAL;
    }
    if signal_mask != 0 && signal_set_size != 8 {
        return -errno::EINVAL;
    }
    let epoll = match epoll_fd(epoll_fd_number) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let deadline = if timeout_ms < 0 {
        None
    } else {
        crate::timer::get_time_ns().checked_add(timeout_ms as u64 * 1_000_000)
    };
    let task = current_task().unwrap();
    let temporary_mask = if signal_mask == 0 {
        false
    } else {
        let mut bytes = [0u8; 8];
        if task.copy_from_user(signal_mask, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        task.begin_signal_suspend(u64::from_ne_bytes(bytes));
        true
    };
    loop {
        let evaluation = match evaluate(&epoll, maximum) {
            Ok(value) => value,
            Err(error) => return error,
        };
        if !evaluation.ready.is_empty() {
            for (index, ready) in evaluation.ready.iter().enumerate() {
                let mut encoded = [0u8; 16];
                encoded[..4].copy_from_slice(&ready.flags.to_ne_bytes());
                encoded[8..].copy_from_slice(&ready.data.to_ne_bytes());
                let Some(address) = events.checked_add(index * 16) else {
                    return -errno::EFAULT;
                };
                if task.copy_to_user(address, &encoded).is_err() {
                    return -errno::EFAULT;
                }
                epoll.commit_delivery(ready.fd, ready.flags, ready.edge, ready.oneshot);
            }
            if temporary_mask {
                task.restore_temporary_signal_mask().unwrap();
            }
            return evaluation.ready.len() as isize;
        }
        if deadline.is_some_and(|value| value <= crate::timer::get_time_ns()) {
            if temporary_mask {
                task.restore_temporary_signal_mask().unwrap();
            }
            return 0;
        }
        match wait_for_poll(evaluation.keys, deadline, || {
            evaluate(&epoll, 1).is_ok_and(|evaluation| !evaluation.ready.is_empty())
        }) {
            WaitResult::Woken => {}
            WaitResult::TimedOut => {
                if temporary_mask {
                    task.restore_temporary_signal_mask().unwrap();
                }
                return 0;
            }
            WaitResult::Interrupted => return -errno::EINTR,
        }
    }
}
