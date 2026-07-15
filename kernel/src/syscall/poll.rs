use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{CharacterDevice, OpenFileDescription, OpenFileKind},
    syscall::errno,
    task::{
        PollWaitKey, TaskControlBlock, WaitResult, current_task, drain_terminal_input,
        wait_for_poll,
    },
};

use super::timer::{TimeSpec, decode_timespec};

mod select;
mod wait_keys;
pub(super) use wait_keys::{PollWaitGuards, PollWaitKeys};

const POLLNVAL: i16 = 0x020;
const POLLIN: i16 = 0x001;
const POLLPRI: i16 = 0x002;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

struct PollDescriptor {
    address: usize,
    fd: i32,
    events: i16,
    revents: i16,
    ofd: Option<Arc<OpenFileDescription>>,
}

fn descriptor_revents(descriptor: &PollDescriptor) -> i16 {
    if descriptor.fd < 0 {
        return 0;
    }
    let Some(ofd) = &descriptor.ofd else {
        return POLLNVAL;
    };
    ofd.poll_events(descriptor.events)
}

fn collect_wait_keys(
    descriptors: &[PollDescriptor],
) -> Result<(Vec<PollWaitKey>, PollWaitGuards), ()> {
    let mut keys = PollWaitKeys::new();
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd {
            keys.add_interest(ofd, descriptor.events | 0x008 | 0x010, false, None)?;
        }
    }
    Ok(keys.finish())
}

fn evaluate(descriptors: &mut [PollDescriptor]) -> usize {
    let mut count = 0;
    for descriptor in descriptors {
        descriptor.revents = descriptor_revents(descriptor);
        if descriptor.revents != 0 {
            count += 1;
        }
    }
    count
}

fn any_ready(descriptors: &[PollDescriptor]) -> bool {
    descriptors
        .iter()
        .any(|descriptor| descriptor_revents(descriptor) != 0)
}

/// @description adapter preparation 遇到 console I/O 错误时的 caller policy。
#[derive(Clone, Copy)]
pub(super) enum PrepareIo {
    Ignore,
    Propagate,
}

/// @description source preparation 在 publication 前可返回的稳定失败类别。
pub(super) enum PrepareError {
    Io,
    NoMemory,
}

fn copy_revents(task: &TaskControlBlock, descriptors: &[PollDescriptor]) -> Result<usize, ()> {
    let mut count = 0;
    for descriptor in descriptors {
        if descriptor.revents != 0 {
            count += 1;
        }
        task.copy_to_user(descriptor.address + 6, &descriptor.revents.to_ne_bytes())
            .map_err(|_| ())?;
    }
    Ok(count)
}

fn prepare_descriptors(descriptors: &[PollDescriptor]) -> Result<(), ()> {
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd {
            prepare_wait_sources(ofd, PrepareIo::Ignore).map_err(|_| ())?;
        }
    }
    Ok(())
}

fn prepare_descriptors_or_restore(
    task: &TaskControlBlock,
    descriptors: &[PollDescriptor],
    temporary_mask: bool,
) -> Result<(), isize> {
    prepare_descriptors(descriptors).map_err(|()| {
        if temporary_mask {
            task.restore_temporary_signal_mask()
                .expect("poll temporary mask disappeared");
        }
        -errno::ENOMEM
    })
}

/// @description 在 wait-key snapshot 后准备同一 OFD tree 的 concrete adapters。
/// @param ofd source tree root。
/// @param io console adapter 失败由 epoll 传播，poll/direct blocking 保持既有忽略 policy。
/// @return preparation 完成，或 snapshot OOM/被要求传播的 console I/O 错误。
pub(super) fn prepare_wait_sources(
    ofd: &Arc<OpenFileDescription>,
    io: PrepareIo,
) -> Result<(), PrepareError> {
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { terminal, pty, .. }) => {
            if let Some(slave) = pty {
                let _ = slave.prepare_to_block();
            } else if drain_terminal_input(terminal).is_err() && matches!(io, PrepareIo::Propagate)
            {
                return Err(PrepareError::Io);
            }
        }
        OpenFileKind::Character(CharacterDevice::Input { file, .. }) => {
            let _ = file.prepare_to_block();
        }
        OpenFileKind::Character(CharacterDevice::Drm(file)) => {
            let _ = file.prepare_to_block();
        }
        OpenFileKind::Character(CharacterDevice::PtyMaster(master)) => {
            let _ = master.prepare_to_block();
        }
        OpenFileKind::Epoll(epoll) => {
            for interest in epoll.snapshot().map_err(|()| PrepareError::NoMemory)? {
                prepare_wait_sources(&interest.ofd, io)?;
            }
        }
        OpenFileKind::Socket(socket) => socket.prepare_wait(),
        _ => {}
    }
    Ok(())
}

/// @description 通过统一 wait registry 等待一个 OFD 达到指定 level readiness。
///
/// @param ofd 要等待的唯一 open-file description。
/// @param events Linux poll event mask。
/// @return source wake、signal interruption；无 deadline，因此不会 timeout。
pub(super) fn wait_for_ofd(ofd: &Arc<OpenFileDescription>, events: i16) -> WaitResult {
    let mut keys = PollWaitKeys::new();
    if keys.add_interest(ofd, i16::MAX, false, None).is_err() {
        return WaitResult::OutOfMemory;
    }
    let (keys, guards) = keys.finish();
    if prepare_wait_sources(ofd, PrepareIo::Ignore).is_err() {
        return WaitResult::OutOfMemory;
    }
    wait_for_poll(keys, None, || {
        guards.changed() || ofd.poll_events(events) != 0
    })
}

/// @description 实现 Linux RV64 ppoll 的 fd readiness、timeout 与临时 signal mask。
///
/// @param poll_fds userspace 8-byte `struct pollfd` 数组。
/// @param count descriptor 数量，受当前 fd-table capacity 约束。
/// @param timeout 可选 relative monotonic timespec。
/// @param signal_mask 可选 8-byte 临时 mask。
/// @param signal_set_size signal_mask 非空时必须为 8。
/// @return ready fd 数、零 timeout，或负 errno。
pub(crate) fn sys_ppoll(
    poll_fds: usize,
    count: usize,
    timeout: usize,
    signal_mask: usize,
    signal_set_size: usize,
) -> isize {
    if count != 0 && poll_fds == 0 {
        return -errno::EINVAL;
    }
    if signal_mask != 0 && signal_set_size != 8 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("ppoll requires current task");
    if count > task.file_descriptor_limit() {
        return -errno::EINVAL;
    }
    let mut descriptors = Vec::new();
    if descriptors.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    for index in 0..count {
        let Some(address) = index
            .checked_mul(8)
            .and_then(|offset| poll_fds.checked_add(offset))
        else {
            return -errno::EFAULT;
        };
        let mut bytes = [0u8; 8];
        if task.copy_from_user(address, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let fd = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
        descriptors.push(PollDescriptor {
            address,
            fd,
            events: i16::from_ne_bytes(bytes[4..6].try_into().unwrap()),
            revents: 0,
            ofd: (fd >= 0).then(|| task.fd_get(fd as usize)).flatten(),
        });
    }
    let deadline = if timeout == 0 {
        None
    } else {
        let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
        if task.copy_from_user(timeout, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let value = decode_timespec(&bytes);
        if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
            return -errno::EINVAL;
        }
        let Some(relative) = value
            .tv_sec
            .checked_mul(1_000_000_000)
            .and_then(|seconds| seconds.checked_add(value.tv_nsec))
            .and_then(|value| u64::try_from(value).ok())
        else {
            return -errno::EINVAL;
        };
        let Some(deadline) = crate::timer::get_time_ns().checked_add(relative) else {
            return -errno::EINVAL;
        };
        Some(deadline)
    };
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
        if let Err(error) = prepare_descriptors_or_restore(&task, &descriptors, temporary_mask) {
            return error;
        }
        let ready = evaluate(&mut descriptors);
        if ready != 0 || deadline.is_some_and(|value| value <= crate::timer::get_time_ns()) {
            if temporary_mask {
                task.restore_temporary_signal_mask()
                    .expect("ppoll temporary mask disappeared");
            }
            return copy_revents(&task, &descriptors)
                .map_or(-errno::EFAULT, |count| count as isize);
        }
        let keys = match collect_wait_keys(&descriptors) {
            Ok(keys) => keys,
            Err(()) => {
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("ppoll temporary mask disappeared");
                }
                return -errno::ENOMEM;
            }
        };
        let (keys, guards) = keys;
        // Key generations were captured first; adapter preparation that
        // observes a concurrent nested ctl cannot make those keys silently
        // stale because the registry-lock guard will detect the change.
        if let Err(error) = prepare_descriptors_or_restore(&task, &descriptors, temporary_mask) {
            return error;
        }
        match wait_for_poll(keys, deadline, || {
            guards.changed() || any_ready(&descriptors)
        }) {
            WaitResult::Woken => {}
            WaitResult::TimedOut => {
                evaluate(&mut descriptors);
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("ppoll temporary mask disappeared");
                }
                return copy_revents(&task, &descriptors)
                    .map_or(-errno::EFAULT, |count| count as isize);
            }
            WaitResult::Interrupted => return -errno::EINTR,
            WaitResult::OutOfMemory => {
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("ppoll temporary mask disappeared");
                }
                return -errno::ENOMEM;
            }
        }
    }
}

/// @description 实现 Linux RV64 `pselect6` fd-set readiness、timeout 与原子临时 signal mask。
/// @param count 检查的 fd 上界，不得超过 fd table capacity。
/// @param read_set 可选 read fd bitmap。
/// @param write_set 可选 write fd bitmap。
/// @param except_set 可选 exceptional-condition fd bitmap。
/// @param timeout 可选 relative monotonic timespec。
/// @param signal_argument 可选 `{ mask pointer, sigset size }` pair。
/// @return 至少在一个输出集合中就绪的 fd 数、零 timeout，或 Linux 负 errno。
pub(crate) fn sys_pselect6(
    count: usize,
    read_set: usize,
    write_set: usize,
    except_set: usize,
    timeout: usize,
    signal_argument: usize,
) -> isize {
    let task = current_task().expect("pselect6 requires current task");
    if count > task.file_descriptor_limit() {
        return -errno::EINVAL;
    }
    let sets = match select::SelectSets::load(&task, count, [read_set, write_set, except_set]) {
        Ok(sets) => sets,
        Err(error) => return error,
    };
    let mut descriptors = Vec::new();
    if descriptors.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    for fd in 0..count {
        let events = sets.events(fd);
        if events == 0 {
            continue;
        }
        let Some(ofd) = task.fd_get(fd) else {
            return -errno::EBADF;
        };
        descriptors.push(PollDescriptor {
            address: 0,
            fd: fd as i32,
            events,
            revents: 0,
            ofd: Some(ofd),
        });
    }
    let deadline = match select::deadline(&task, timeout) {
        Ok(deadline) => deadline,
        Err(error) => return error,
    };
    let temporary_mask = match select::install_signal_mask(&task, signal_argument) {
        Ok(temporary) => temporary,
        Err(error) => return error,
    };
    loop {
        if let Err(error) = prepare_descriptors_or_restore(&task, &descriptors, temporary_mask) {
            return error;
        }
        evaluate(&mut descriptors);
        if descriptors.iter().any(|descriptor| descriptor.revents != 0)
            || deadline.is_some_and(|value| value <= crate::timer::get_time_ns())
        {
            if temporary_mask {
                task.restore_temporary_signal_mask()
                    .expect("pselect6 temporary mask disappeared");
            }
            return sets.copy_results(
                &task,
                descriptors.iter().map(|descriptor| {
                    (
                        descriptor.fd as usize,
                        descriptor.events,
                        descriptor.revents,
                    )
                }),
            );
        }
        let keys = match collect_wait_keys(&descriptors) {
            Ok(keys) => keys,
            Err(()) => {
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("pselect6 temporary mask disappeared");
                }
                return -errno::ENOMEM;
            }
        };
        let (keys, guards) = keys;
        if let Err(error) = prepare_descriptors_or_restore(&task, &descriptors, temporary_mask) {
            return error;
        }
        match wait_for_poll(keys, deadline, || {
            guards.changed() || any_ready(&descriptors)
        }) {
            WaitResult::Woken => {}
            WaitResult::TimedOut => {
                evaluate(&mut descriptors);
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("pselect6 temporary mask disappeared");
                }
                return sets.copy_results(
                    &task,
                    descriptors.iter().map(|descriptor| {
                        (
                            descriptor.fd as usize,
                            descriptor.events,
                            descriptor.revents,
                        )
                    }),
                );
            }
            WaitResult::Interrupted => return -errno::EINTR,
            WaitResult::OutOfMemory => {
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("pselect6 temporary mask disappeared");
                }
                return -errno::ENOMEM;
            }
        }
    }
}
