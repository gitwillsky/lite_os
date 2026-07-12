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

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
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
    match &ofd.kind {
        OpenFileKind::Inode(_) => descriptor.events & (POLLIN | POLLOUT),
        OpenFileKind::Character(device) => match device {
            CharacterDevice::Null | CharacterDevice::Zero => descriptor.events & (POLLIN | POLLOUT),
            CharacterDevice::Terminal { terminal, .. } => {
                let mut result = descriptor.events & POLLOUT;
                if descriptor.events & POLLIN != 0 && terminal.wait_ready() {
                    result |= POLLIN;
                }
                result
            }
        },
        OpenFileKind::Pipe(endpoint) => {
            let state = endpoint.pipe().poll_state(endpoint.direction());
            let mut result = 0;
            if descriptor.events & POLLIN != 0 && state.readable {
                result |= POLLIN;
            }
            if descriptor.events & POLLOUT != 0 && state.writable {
                result |= POLLOUT;
            }
            if state.error {
                result |= POLLERR;
            }
            if state.hangup {
                result |= POLLHUP;
            }
            result
        }
    }
}

fn collect_wait_keys(descriptors: &[PollDescriptor]) -> Vec<PollWaitKey> {
    let mut keys = Vec::new();
    for descriptor in descriptors {
        let Some(ofd) = &descriptor.ofd else {
            continue;
        };
        match &ofd.kind {
            OpenFileKind::Character(CharacterDevice::Terminal { .. })
                if descriptor.events & POLLIN != 0 =>
            {
                keys.push(PollWaitKey::Console);
            }
            OpenFileKind::Pipe(endpoint) => {
                keys.push(PollWaitKey::pipe(&endpoint.pipe(), endpoint.direction()));
            }
            _ => {}
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

fn drain_terminals(descriptors: &[PollDescriptor]) {
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd
            && let OpenFileKind::Character(CharacterDevice::Terminal { terminal, .. }) = &ofd.kind
        {
            let _ = drain_terminal_input(terminal);
        }
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
        drain_terminals(&descriptors);
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
