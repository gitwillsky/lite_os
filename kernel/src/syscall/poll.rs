use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{CharacterDevice, OpenFileDescription, OpenFileKind},
    socket::SocketSendBlocker,
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

fn copy_revents(
    task: &TaskControlBlock,
    poll_fds: usize,
    raw: &mut [u8],
    descriptors: &[PollDescriptor],
) -> Result<usize, ()> {
    let mut count = 0;
    for (index, descriptor) in descriptors.iter().enumerate() {
        if descriptor.revents != 0 {
            count += 1;
        }
        let offset = index * 8 + 6;
        raw[offset..offset + 2].copy_from_slice(&descriptor.revents.to_ne_bytes());
    }
    task.copy_to_user(poll_fds, raw).map_err(|_| ())?;
    Ok(count)
}

fn prepare_descriptors(descriptors: &[PollDescriptor]) {
    for descriptor in descriptors {
        if let Some(ofd) = &descriptor.ofd {
            prepare_wait_sources(ofd);
        }
    }
}

/// @description 在 wait-key snapshot 后准备同一 OFD tree 的 concrete adapters。
/// @param ofd source tree root。
/// @return 无返回值；adapter preparation 不分配。
pub(super) fn prepare_wait_sources(ofd: &Arc<OpenFileDescription>) {
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
        // epoll 的持久 source index 已由 ctl 路径准备；poll 只等待
        // epoll 自身 notification，不重建嵌套 interest tree。
        OpenFileKind::Epoll(_) => {}
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
    let mut keys = PollWaitKeys::new();
    if keys.add_interest(ofd, i16::MAX, false, None).is_err() {
        return WaitResult::OutOfMemory;
    }
    let (keys, guards) = keys.finish();
    prepare_wait_sources(ofd);
    wait_for_poll(keys, None, || {
        guards.changed() || ofd.poll_events(events) != 0
    })
}

/// @description 等待一次 AF_UNIX datagram send 的具体 target queue 恢复容量。
/// @param blocker socket facade 持有的 opaque target projection。
/// @return source wake、signal interruption或 wait-key allocation failure。
pub(super) fn wait_for_socket_send(blocker: &SocketSendBlocker) -> WaitResult {
    let mut keys = PollWaitKeys::new();
    if keys
        .add_socket_source(blocker.wait_source(), POLLOUT, false, None)
        .is_err()
    {
        return WaitResult::OutOfMemory;
    }
    let (keys, guards) = keys.finish();
    blocker.prepare_wait();
    wait_for_poll(keys, None, || guards.changed() || blocker.is_ready())
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
    let Some(raw_length) = count.checked_mul(8) else {
        return -errno::EFAULT;
    };
    if poll_fds.checked_add(raw_length).is_none() {
        return -errno::EFAULT;
    }
    let mut raw = Vec::new();
    if raw.try_reserve_exact(raw_length).is_err() {
        return -errno::ENOMEM;
    }
    raw.resize(raw_length, 0);
    if task.copy_from_user(poll_fds, &mut raw).is_err() {
        return -errno::EFAULT;
    }
    let mut descriptors = Vec::new();
    if descriptors.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    let (poll_entries, remainder) = raw.as_chunks::<8>();
    debug_assert!(remainder.is_empty());
    for bytes in poll_entries {
        let fd = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
        descriptors.push(PollDescriptor {
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
            return copy_revents(&task, poll_fds, &mut raw, &descriptors)
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
        prepare_descriptors(&descriptors);
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
                return copy_revents(&task, poll_fds, &mut raw, &descriptors)
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
        let (keys, guards) = keys;
        prepare_descriptors(&descriptors);
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
