use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{CharacterDevice, OpenFileDescription, OpenFileKind},
    socket::SocketWaitSource,
    syscall::errno,
    task::{
        PollWaitKey, TaskControlBlock, WaitResult, current_task, drain_terminal_input,
        wait_for_poll,
    },
};

use super::timer::{TimeSpec, decode_timespec};

mod select;

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

fn ofd_wait_keys(ofd: &Arc<OpenFileDescription>) -> Result<Vec<PollWaitKey>, ()> {
    ofd_wait_keys_for_interest(ofd, i16::MAX, false, None)
}

pub(super) fn ofd_wait_keys_for_interest(
    ofd: &Arc<OpenFileDescription>,
    events: i16,
    exclusive: bool,
    wake_group: Option<usize>,
) -> Result<Vec<PollWaitKey>, ()> {
    let mut keys = Vec::new();
    let push = |keys: &mut Vec<PollWaitKey>, key| {
        keys.try_reserve(1).map_err(|_| ())?;
        keys.push(key);
        Ok::<(), ()>(())
    };
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { pty, .. }) => {
            if let Some(slave) = pty {
                push(
                    &mut keys,
                    PollWaitKey::pipe(
                        &slave.notification_pipe(),
                        crate::ipc::PipeDirection::Read,
                        POLLIN,
                        exclusive,
                        wake_group,
                    ),
                )?;
                if events & POLLOUT != 0 {
                    push(
                        &mut keys,
                        PollWaitKey::pipe(
                            &slave.output_pipe(),
                            crate::ipc::PipeDirection::Write,
                            events,
                            exclusive,
                            wake_group,
                        ),
                    )?;
                }
            } else {
                push(
                    &mut keys,
                    PollWaitKey::console(events, exclusive, wake_group),
                )?;
            }
        }
        OpenFileKind::Character(CharacterDevice::Input { file, .. }) => {
            push(
                &mut keys,
                PollWaitKey::pipe(
                    &file.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    POLLIN,
                    exclusive,
                    wake_group,
                ),
            )?;
        }
        OpenFileKind::Character(CharacterDevice::Drm(file)) => {
            push(
                &mut keys,
                PollWaitKey::pipe(
                    &file.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    POLLIN,
                    exclusive,
                    wake_group,
                ),
            )?;
        }
        OpenFileKind::Character(CharacterDevice::PtyMaster(master)) => {
            push(
                &mut keys,
                PollWaitKey::pipe(
                    &master.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    POLLIN | POLLOUT | POLLHUP,
                    exclusive,
                    wake_group,
                ),
            )?;
        }
        OpenFileKind::Pipe(endpoint) => {
            push(
                &mut keys,
                PollWaitKey::pipe(
                    &endpoint.pipe(),
                    endpoint.direction(),
                    events,
                    exclusive,
                    wake_group,
                ),
            )?;
        }
        OpenFileKind::Socket(socket) => {
            for source in socket.wait_sources().into_iter().flatten() {
                push(
                    &mut keys,
                    match source {
                        SocketWaitSource::Notification(pipe) => PollWaitKey::pipe(
                            &pipe,
                            crate::ipc::PipeDirection::Read,
                            POLLIN,
                            exclusive,
                            wake_group,
                        ),
                        SocketWaitSource::Data { pipe, direction } => {
                            PollWaitKey::pipe(&pipe, direction, events, exclusive, wake_group)
                        }
                    },
                )?;
            }
        }
        OpenFileKind::Epoll(epoll) => {
            debug_assert!(!exclusive, "EPOLLEXCLUSIVE cannot target an epoll fd");
            push(
                &mut keys,
                PollWaitKey::pipe(
                    &epoll.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    0x001,
                    false,
                    wake_group,
                ),
            )?;
            for interest in epoll.snapshot().map_err(|_| ())? {
                let mut nested = ofd_wait_keys_for_interest(
                    &interest.ofd,
                    interest.event.events as i16,
                    interest.event.events & (1 << 28) != 0,
                    wake_group,
                )?;
                keys.try_reserve(nested.len()).map_err(|_| ())?;
                keys.append(&mut nested);
            }
        }
        OpenFileKind::EventFd(event) => {
            if events & POLLIN != 0 {
                push(
                    &mut keys,
                    PollWaitKey::pipe(
                        &event.notification_pipe(true),
                        crate::ipc::PipeDirection::Read,
                        POLLIN,
                        exclusive,
                        wake_group,
                    ),
                )?;
            }
            if events & POLLOUT != 0 {
                push(
                    &mut keys,
                    PollWaitKey::pipe(
                        &event.notification_pipe(false),
                        crate::ipc::PipeDirection::Read,
                        POLLOUT,
                        exclusive,
                        wake_group,
                    ),
                )?;
            }
        }
        _ => {}
    }
    Ok(keys)
}

fn collect_wait_keys(descriptors: &[PollDescriptor]) -> Result<Vec<PollWaitKey>, ()> {
    let mut keys = Vec::new();
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd {
            let mut nested =
                ofd_wait_keys_for_interest(ofd, descriptor.events | 0x008 | 0x010, false, None)?;
            keys.try_reserve(nested.len()).map_err(|_| ())?;
            keys.append(&mut nested);
        }
    }
    Ok(keys)
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

fn prepare_descriptors(descriptors: &[PollDescriptor]) {
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd {
            prepare_ofd(ofd);
        }
    }
}

fn prepare_ofd(ofd: &Arc<OpenFileDescription>) {
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { terminal, pty, .. }) => {
            if let Some(slave) = pty {
                let _ = slave.prepare_to_block();
            } else {
                let _ = drain_terminal_input(terminal);
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
            epoll.consume_notifications();
            if let Ok(entries) = epoll.snapshot() {
                for interest in entries {
                    prepare_ofd(&interest.ofd);
                }
            }
        }
        OpenFileKind::Socket(socket) => socket.prepare_wait(),
        _ => {}
    }
}

/// @description 通过统一 wait registry 等待一个 OFD 达到指定 level readiness。
///
/// @param ofd 要等待的唯一 open-file description。
/// @param events Linux poll event mask。
/// @return source wake、signal interruption；无 deadline，因此不会 timeout。
pub(super) fn wait_for_ofd(ofd: &Arc<OpenFileDescription>, events: i16) -> WaitResult {
    let Ok(keys) = ofd_wait_keys(ofd) else {
        return WaitResult::OutOfMemory;
    };
    wait_for_poll(keys, None, || {
        prepare_ofd(ofd);
        ofd.poll_events(events) != 0
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
        prepare_descriptors(&descriptors);
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
        match wait_for_poll(keys, deadline, || {
            prepare_descriptors(&descriptors);
            any_ready(&descriptors)
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
        prepare_descriptors(&descriptors);
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
        match wait_for_poll(keys, deadline, || {
            prepare_descriptors(&descriptors);
            any_ready(&descriptors)
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
