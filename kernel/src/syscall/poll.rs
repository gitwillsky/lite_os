use alloc::{sync::Arc, vec, vec::Vec};

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

pub(super) fn ofd_wait_keys(ofd: &Arc<OpenFileDescription>) -> Vec<PollWaitKey> {
    ofd_wait_keys_for_interest(ofd, i16::MAX, false, None)
}

pub(super) fn ofd_wait_keys_for_interest(
    ofd: &Arc<OpenFileDescription>,
    events: i16,
    exclusive: bool,
    wake_group: Option<usize>,
) -> Vec<PollWaitKey> {
    let mut keys = Vec::new();
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { .. }) => {
            keys.push(PollWaitKey::console(events, exclusive, wake_group))
        }
        OpenFileKind::Pipe(endpoint) => keys.push(PollWaitKey::pipe(
            &endpoint.pipe(),
            endpoint.direction(),
            events,
            exclusive,
            wake_group,
        )),
        OpenFileKind::Socket(socket) => {
            keys.extend(socket.wait_pipes().into_iter().map(|(pipe, direction)| {
                PollWaitKey::pipe(&pipe, direction, events, exclusive, wake_group)
            }));
        }
        OpenFileKind::Epoll(epoll) => {
            debug_assert!(!exclusive, "EPOLLEXCLUSIVE cannot target an epoll fd");
            keys.push(PollWaitKey::pipe(
                &epoll.notification_pipe(),
                crate::ipc::PipeDirection::Read,
                0x001,
                false,
                wake_group,
            ));
            if let Ok(entries) = epoll.snapshot() {
                for interest in entries {
                    keys.extend(ofd_wait_keys_for_interest(
                        &interest.ofd,
                        interest.event.events as i16,
                        interest.event.events & (1 << 28) != 0,
                        wake_group,
                    ));
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
            keys.extend(ofd_wait_keys_for_interest(
                ofd,
                descriptor.events | 0x008 | 0x010,
                false,
                None,
            ));
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
    if count > MAX_FILE_DESCRIPTORS {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pselect6 requires current task");
    let byte_count = count.div_ceil(8);
    let read_bits = match copy_fd_set(&task, read_set, byte_count) {
        Ok(bits) => bits,
        Err(error) => return error,
    };
    let write_bits = match copy_fd_set(&task, write_set, byte_count) {
        Ok(bits) => bits,
        Err(error) => return error,
    };
    let except_bits = match copy_fd_set(&task, except_set, byte_count) {
        Ok(bits) => bits,
        Err(error) => return error,
    };
    let mut descriptors = Vec::new();
    if descriptors.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    for fd in 0..count {
        let mut events = 0;
        if fd_is_set(&read_bits, fd) {
            events |= POLLIN;
        }
        if fd_is_set(&write_bits, fd) {
            events |= POLLOUT;
        }
        if fd_is_set(&except_bits, fd) {
            events |= POLLPRI;
        }
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
    let deadline = match select_deadline(&task, timeout) {
        Ok(deadline) => deadline,
        Err(error) => return error,
    };
    let temporary_mask = match select_signal_mask(&task, signal_argument) {
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
            return copy_select_results(
                &task,
                count,
                read_set,
                write_set,
                except_set,
                &descriptors,
            );
        }
        let keys = collect_wait_keys(&descriptors);
        match wait_for_poll(keys, deadline, || any_ready(&descriptors)) {
            WaitResult::Woken => {}
            WaitResult::TimedOut => {
                evaluate(&mut descriptors);
                if temporary_mask {
                    task.restore_temporary_signal_mask()
                        .expect("pselect6 temporary mask disappeared");
                }
                return copy_select_results(
                    &task,
                    count,
                    read_set,
                    write_set,
                    except_set,
                    &descriptors,
                );
            }
            WaitResult::Interrupted => return -errno::EINTR,
        }
    }
}

fn copy_fd_set(
    task: &TaskControlBlock,
    address: usize,
    byte_count: usize,
) -> Result<Vec<u8>, isize> {
    if address == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_count)
        .map_err(|_| -errno::ENOMEM)?;
    bytes.resize(byte_count, 0);
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    Ok(bytes)
}

fn fd_is_set(bits: &[u8], fd: usize) -> bool {
    bits.get(fd / 8)
        .is_some_and(|byte| byte & (1 << (fd % 8)) != 0)
}

fn select_deadline(task: &TaskControlBlock, timeout: usize) -> Result<Option<u64>, isize> {
    if timeout == 0 {
        return Ok(None);
    }
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    task.copy_from_user(timeout, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    let value = decode_timespec(&bytes);
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(-errno::EINVAL);
    }
    let relative = value
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(value.tv_nsec))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(-errno::EINVAL)?;
    crate::timer::get_time_ns()
        .checked_add(relative)
        .map(Some)
        .ok_or(-errno::EINVAL)
}

fn select_signal_mask(task: &TaskControlBlock, argument: usize) -> Result<bool, isize> {
    if argument == 0 {
        return Ok(false);
    }
    let mut pair = [0u8; 16];
    task.copy_from_user(argument, &mut pair)
        .map_err(|_| -errno::EFAULT)?;
    let mask = usize::from_ne_bytes(pair[..8].try_into().unwrap());
    let size = usize::from_ne_bytes(pair[8..].try_into().unwrap());
    if size != 8 {
        return Err(-errno::EINVAL);
    }
    if mask == 0 {
        return Ok(false);
    }
    let mut bytes = [0u8; 8];
    task.copy_from_user(mask, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    task.begin_signal_suspend(u64::from_ne_bytes(bytes));
    Ok(true)
}

fn copy_select_results(
    task: &TaskControlBlock,
    count: usize,
    read_set: usize,
    write_set: usize,
    except_set: usize,
    descriptors: &[PollDescriptor],
) -> isize {
    let byte_count = count.div_ceil(8);
    let mut read_bits = vec![0u8; byte_count];
    let mut write_bits = vec![0u8; byte_count];
    let mut except_bits = vec![0u8; byte_count];
    let mut ready = 0;
    for descriptor in descriptors {
        let fd = descriptor.fd as usize;
        let mut descriptor_ready = false;
        if descriptor.events & POLLIN != 0 && descriptor.revents & (POLLIN | POLLERR | POLLHUP) != 0
        {
            read_bits[fd / 8] |= 1 << (fd % 8);
            descriptor_ready = true;
        }
        if descriptor.events & POLLOUT != 0 && descriptor.revents & (POLLOUT | POLLERR) != 0 {
            write_bits[fd / 8] |= 1 << (fd % 8);
            descriptor_ready = true;
        }
        if descriptor.events & POLLPRI != 0 && descriptor.revents & POLLPRI != 0 {
            except_bits[fd / 8] |= 1 << (fd % 8);
            descriptor_ready = true;
        }
        if descriptor_ready {
            ready += 1;
        }
    }
    for (address, bits) in [
        (read_set, read_bits.as_slice()),
        (write_set, write_bits.as_slice()),
        (except_set, except_bits.as_slice()),
    ] {
        if address != 0 && task.copy_to_user(address, bits).is_err() {
            return -errno::EFAULT;
        }
    }
    ready
}
