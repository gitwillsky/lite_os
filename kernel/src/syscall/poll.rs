use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{CharacterDevice, MAX_FILE_DESCRIPTORS, OpenFileDescription, OpenFileKind},
    syscall::errno,
    task::{
        PollWaitKey, TaskControlBlock, WaitResult, current_task, drain_terminal_input,
        wait_for_poll,
    },
};

use super::timer::{TimeSpec, decode_timespec};

const POLLNVAL: i16 = 0x020;

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

pub(super) fn ofd_wait_keys(ofd: &Arc<OpenFileDescription>) -> Vec<PollWaitKey> {
    let mut keys = Vec::new();
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { .. }) => {
            keys.push(PollWaitKey::Console)
        }
        OpenFileKind::Pipe(endpoint) => {
            keys.push(PollWaitKey::pipe(&endpoint.pipe(), endpoint.direction()))
        }
        OpenFileKind::Socket(socket) => {
            keys.extend(
                socket
                    .wait_pipes()
                    .into_iter()
                    .map(|(pipe, direction)| PollWaitKey::pipe(&pipe, direction)),
            );
        }
        OpenFileKind::Epoll(epoll) => {
            keys.push(PollWaitKey::pipe(
                &epoll.notification_pipe(),
                crate::ipc::PipeDirection::Read,
            ));
            if let Ok(entries) = epoll.snapshot() {
                for interest in entries {
                    keys.extend(ofd_wait_keys(&interest.ofd));
                }
            }
        }
        _ => {}
    }
    keys
}

fn collect_wait_keys(descriptors: &[PollDescriptor]) -> Vec<PollWaitKey> {
    let mut keys = Vec::new();
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd {
            keys.extend(ofd_wait_keys(ofd));
        }
    }
    keys
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
        OpenFileKind::Character(CharacterDevice::Terminal { terminal, .. }) => {
            let _ = drain_terminal_input(terminal);
        }
        OpenFileKind::Epoll(epoll) => {
            epoll.consume_notifications();
            if let Ok(entries) = epoll.snapshot() {
                for interest in entries {
                    prepare_ofd(&interest.ofd);
                }
            }
        }
        _ => {}
    }
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
    if count > MAX_FILE_DESCRIPTORS || count != 0 && poll_fds == 0 {
        return -errno::EINVAL;
    }
    if signal_mask != 0 && signal_set_size != 8 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("ppoll requires current task");
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
        let keys = collect_wait_keys(&descriptors);
        match wait_for_poll(keys, deadline, || any_ready(&descriptors)) {
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
        }
    }
}
