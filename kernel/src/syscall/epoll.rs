use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{CharacterDevice, Epoll, EpollChange, EpollEvent, OpenFileDescription, OpenFileKind},
    task::{WaitResult, create_pipe_endpoints, current_task, drain_terminal_input, wait_for_poll},
};

use super::{errno, poll::ofd_wait_keys};

const EPOLL_CLOEXEC: usize = 0x80000;
const EPOLL_CTL_ADD: usize = 1;
const EPOLL_CTL_DEL: usize = 2;
const EPOLL_CTL_MOD: usize = 3;
const EPOLLEXCLUSIVE: u32 = 1 << 28;
const EPOLLWAKEUP: u32 = 1 << 29;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
const EPOLL_ALLOWED: u32 =
    0x001 | 0x002 | 0x004 | 0x008 | 0x010 | 0x2000 | EPOLLET | EPOLLONESHOT | EPOLLWAKEUP;

struct ReadyEvent {
    fd: usize,
    ofd: Arc<OpenFileDescription>,
    revision: u64,
    generation: u64,
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
    let mut event = EpollEvent {
        events: u32::from_ne_bytes(bytes[..4].try_into().unwrap()),
        data: u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
    };
    if event.events & (EPOLLEXCLUSIVE | !EPOLL_ALLOWED) != 0 {
        return Err(-errno::EINVAL);
    }
    // LiteOS 没有 suspend/power-management phase，因此与无 CAP_BLOCK_SUSPEND 的 Linux
    // caller 一样忽略 EPOLLWAKEUP；保留该位会虚假表达一个不存在的 wake lock owner。
    event.events &= !EPOLLWAKEUP;
    Ok(event)
}

pub(crate) fn sys_epoll_create1(flags: usize) -> isize {
    if flags & !EPOLL_CLOEXEC != 0 {
        return -errno::EINVAL;
    }
    let (notification_read, notification_write) = match create_pipe_endpoints() {
        Ok(value) => value,
        Err(()) => return -errno::ENOMEM,
    };
    let epoll = match Epoll::new(notification_read, notification_write) {
        Ok(value) => value,
        Err(()) => return -errno::ENOMEM,
    };
    current_task()
        .unwrap()
        .fd_allocate(
            OpenFileDescription::epoll(epoll),
            flags & EPOLL_CLOEXEC != 0,
        )
        .map_or(-errno::EMFILE, |fd| fd as isize)
}

pub(crate) fn sys_epoll_ctl(
    epoll_fd_number: usize,
    operation: usize,
    fd: usize,
    event_pointer: usize,
) -> isize {
    if epoll_fd_number == fd {
        return -errno::EINVAL;
    }
    let epoll = match epoll_fd(epoll_fd_number) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let (change, event) = match operation {
        EPOLL_CTL_ADD => (
            EpollChange::Add,
            match read_event(event_pointer) {
                Ok(value) => Some(value),
                Err(error) => return error,
            },
        ),
        EPOLL_CTL_DEL => (EpollChange::Delete, None),
        EPOLL_CTL_MOD => (
            EpollChange::Modify,
            match read_event(event_pointer) {
                Ok(value) => Some(value),
                Err(error) => return error,
            },
        ),
        _ => return -errno::EINVAL,
    };
    let changed = current_task().unwrap().with_file_descriptions(
        epoll_fd_number,
        fd,
        |epoll_ofd, watched| match &epoll_ofd.kind {
            OpenFileKind::Epoll(current) if Arc::ptr_eq(current, &epoll) => {
                current.change(change, fd, watched, event)
            }
            _ => Err(crate::fs::EpollChangeError::Invalid),
        },
    );
    let Some(changed) = changed else {
        return -errno::EBADF;
    };
    changed.map_or_else(
        |error| -match error {
            crate::fs::EpollChangeError::Exists => errno::EEXIST,
            crate::fs::EpollChangeError::NotFound => errno::ENOENT,
            crate::fs::EpollChangeError::Invalid => errno::EINVAL,
            crate::fs::EpollChangeError::Permission => errno::EPERM,
            crate::fs::EpollChangeError::Loop => errno::ELOOP,
            crate::fs::EpollChangeError::NoMemory => errno::ENOMEM,
        },
        |()| 0,
    )
}

fn evaluate(epoll: &Arc<Epoll>, maximum: usize) -> Result<Evaluation, isize> {
    epoll.consume_notifications();
    let snapshot = epoll.snapshot().map_err(|_| -errno::ENOMEM)?;
    let mut ready = Vec::new();
    let mut keys = Vec::new();
    ready
        .try_reserve_exact(maximum.min(snapshot.len()))
        .map_err(|_| -errno::ENOMEM)?;
    for interest in snapshot {
        drain_terminals(&interest.ofd)?;
        keys.extend(ofd_wait_keys(&interest.ofd));
        if interest.disabled {
            continue;
        }
        let current = interest.ofd.poll_events(interest.event.events as i16) as u32;
        if current == 0 {
            continue;
        }
        let edge = interest.event.events & EPOLLET != 0;
        let generation = interest
            .ofd
            .readiness_generation(interest.event.events as i16);
        if edge && generation == interest.last_generation {
            continue;
        }
        ready.push(ReadyEvent {
            fd: interest.fd,
            ofd: interest.ofd,
            revision: interest.revision,
            generation,
            flags: current,
            data: interest.event.data,
            edge,
            oneshot: interest.event.events & EPOLLONESHOT != 0,
        });
        if ready.len() == maximum {
            break;
        }
    }
    Ok(Evaluation { ready, keys })
}

fn drain_terminals(ofd: &Arc<OpenFileDescription>) -> Result<(), isize> {
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { terminal, .. }) => {
            drain_terminal_input(terminal).map_err(|()| -errno::EIO)
        }
        OpenFileKind::Epoll(epoll) => {
            epoll.consume_notifications();
            for interest in epoll.snapshot().map_err(|()| -errno::ENOMEM)? {
                drain_terminals(&interest.ofd)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn restore_mask_after_error(temporary_mask: bool) {
    if temporary_mask {
        current_task()
            .unwrap()
            .restore_temporary_signal_mask()
            .unwrap();
    }
}

pub(crate) fn sys_epoll_pwait(
    epoll_fd_number: usize,
    events: usize,
    maximum: usize,
    timeout_ms: isize,
    signal_mask: usize,
    signal_set_size: usize,
) -> isize {
    if events == 0
        || maximum == 0
        || maximum > i32::MAX as usize
        || !(-1..=i32::MAX as isize).contains(&timeout_ms)
    {
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
        Some(
            crate::timer::get_time_ns()
                .saturating_add((timeout_ms as u64).saturating_mul(1_000_000)),
        )
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
            Err(error) => {
                restore_mask_after_error(temporary_mask);
                return error;
            }
        };
        if !evaluation.ready.is_empty() {
            let Some(encoded_length) = evaluation.ready.len().checked_mul(16) else {
                restore_mask_after_error(temporary_mask);
                return -errno::ENOMEM;
            };
            let mut encoded = Vec::new();
            if encoded.try_reserve_exact(encoded_length).is_err() {
                restore_mask_after_error(temporary_mask);
                return -errno::ENOMEM;
            }
            encoded.resize(encoded_length, 0);
            for (index, ready) in evaluation.ready.iter().enumerate() {
                let offset = index * 16;
                encoded[offset..offset + 4].copy_from_slice(&ready.flags.to_ne_bytes());
                encoded[offset + 8..offset + 16].copy_from_slice(&ready.data.to_ne_bytes());
            }
            // 单次 copyout 先验证完整范围；EFAULT 时没有任何 ET/ONESHOT state 被消费。
            if task.copy_to_user(events, &encoded).is_err() {
                restore_mask_after_error(temporary_mask);
                return -errno::EFAULT;
            }
            for ready in &evaluation.ready {
                epoll.commit_delivery(
                    ready.fd,
                    &ready.ofd,
                    ready.revision,
                    ready.generation,
                    ready.edge,
                    ready.oneshot,
                );
            }
            restore_mask_after_error(temporary_mask);
            return evaluation.ready.len() as isize;
        }
        if deadline.is_some_and(|value| value <= crate::timer::get_time_ns()) {
            restore_mask_after_error(temporary_mask);
            return 0;
        }
        match wait_for_poll(evaluation.keys, deadline, || {
            evaluate(&epoll, 1).is_ok_and(|evaluation| !evaluation.ready.is_empty())
        }) {
            WaitResult::Woken => {}
            WaitResult::TimedOut => {
                restore_mask_after_error(temporary_mask);
                return 0;
            }
            // signal frame 必须看到 begin_signal_suspend 保存的旧 mask，不能在 EINTR 前抢先清除。
            WaitResult::Interrupted => return -errno::EINTR,
        }
    }
}
