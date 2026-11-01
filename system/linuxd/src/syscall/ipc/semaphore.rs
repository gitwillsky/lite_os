//! asm-generic semaphore syscall codec；操作、等待、权限与撤销均由实际 IPC owner 完成。
use super::{encode_permission, ipc_error};
use crate::{
    execution::task::current_task,
    ipc::{SemaphoreChange, SemaphoreOperation},
};
use alloc::vec::Vec;
use syscall_abi::{errno, ipc::*};

pub(in crate::syscall) fn sys_semget(key: i32, count: i32, flags: u32) -> isize {
    let task = current_task().expect("semget requires calling Thread");
    task.ipc_namespace()
        .semaphore_get(
            key,
            count,
            flags,
            &task.ipc_access(),
            crate::execution::timer::get_realtime_ns() / 1_000_000_000,
        )
        .map_or_else(ipc_error, |id| id as isize)
}
pub(in crate::syscall) fn sys_semop(id: i32, address: usize, count: usize) -> isize {
    sys_semtimedop(id, address, count, 0)
}
pub(in crate::syscall) fn sys_semtimedop(
    id: i32,
    address: usize,
    count: usize,
    timeout: usize,
) -> isize {
    let task = current_task().expect("semaphore operation requires calling Thread");
    // ksys_semtimedop 先复制 timeout，再做 nsops/ops copy；实际 timespec 范围在 ops copy 后检查。
    let timeout = if timeout == 0 {
        None
    } else {
        let mut bytes = [0u8; 16];
        if task.copy_from_user(timeout, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        Some((
            i64::from_ne_bytes(bytes[..8].try_into().unwrap()),
            i64::from_ne_bytes(bytes[8..].try_into().unwrap()),
        ))
    };
    let namespace = task.ipc_namespace();
    if count > namespace.semaphore_limits()[2] as u32 as usize {
        return -errno::E2BIG;
    }
    if count == 0 {
        return -errno::EINVAL;
    }
    let Some(size) = count.checked_mul(6) else {
        return -errno::E2BIG;
    };
    let mut bytes = Vec::new();
    let mut changes = Vec::new();
    if bytes.try_reserve_exact(size).is_err() || changes.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    if task
        .copy_from_user_uninit(address, &mut bytes.spare_capacity_mut()[..size])
        .is_err()
    {
        return -errno::EFAULT;
    }
    // SAFETY: user-copy 完整初始化独占 Vec 的 size 字节，长度只在其成功后发布。
    unsafe {
        bytes.set_len(size);
    }
    for bytes in bytes.as_chunks::<6>().0 {
        changes.push(SemaphoreChange {
            index: u16::from_ne_bytes(bytes[..2].try_into().unwrap()),
            delta: i16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
            flags: u16::from_ne_bytes(bytes[4..6].try_into().unwrap()),
        });
    }
    drop(bytes);
    if id < 0
        || timeout
            .is_some_and(|(seconds, nanos)| seconds < 0 || !(0..1_000_000_000).contains(&nanos))
    {
        return -errno::EINVAL;
    }
    let deadline = timeout.map(|(seconds, nanos)| {
        let duration = (seconds as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(nanos as u64);
        crate::execution::timer::get_time_ns()
            .saturating_add(duration)
            .min(i64::MAX as u64)
    });
    let set = match namespace.semaphore_set(id) {
        Ok(set) => set,
        Err(error) => return ipc_error(error),
    };
    let undo = if changes.iter().any(|change| change.flags & SEM_UNDO != 0) {
        match task.semaphore_undo().entry(&set) {
            Ok(undo) => Some(undo),
            Err(error) => return ipc_error(error),
        }
    } else {
        None
    };
    let mut operation =
        match SemaphoreOperation::new(&set, changes, undo, task.tgid_identity().clone()) {
            Ok(operation) => operation,
            Err(error) => return ipc_error(error),
        };
    match set.operate(
        &mut operation,
        &task.ipc_access(),
        crate::execution::timer::get_realtime_ns() / 1_000_000_000,
    ) {
        Ok(true) => return 0,
        Err(error) => return ipc_error(error),
        Ok(false) => {}
    }
    let waited = operation.wait_until(deadline);
    if let Some(complete) = operation.finish_wait() {
        return complete.map_or_else(ipc_error, |()| 0);
    }
    match waited {
        crate::execution::sync::WaitResult::TimedOut => -errno::EAGAIN,
        crate::execution::sync::WaitResult::OutOfMemory => -errno::ENOMEM,
        // Linux semop/semtimedop 被捕获信号中断始终 EINTR，SA_RESTART 不重放。
        crate::execution::sync::WaitResult::Interrupted => -errno::EINTR,
        crate::execution::sync::WaitResult::Woken => {
            panic!("semaphore wake must publish completion")
        }
    }
}
pub(in crate::syscall) fn sys_semctl(id: i32, index: i32, command: i32, argument: usize) -> isize {
    if id < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("semctl requires calling Thread");
    let namespace = task.ipc_namespace();
    let access = task.ipc_access();
    let now = crate::execution::timer::get_realtime_ns() / 1_000_000_000;
    match command {
        IPC_RMID => namespace
            .semaphore_remove(id, &access)
            .map_or_else(ipc_error, |()| 0),
        IPC_SET => {
            let mut bytes = [0u8; SEM_SET_STAT_BYTES];
            if task.copy_from_user(argument, &mut bytes).is_err() {
                return -errno::EFAULT;
            }
            let uid = u32::from_ne_bytes(bytes[4..8].try_into().unwrap());
            let gid = u32::from_ne_bytes(bytes[8..12].try_into().unwrap());
            let user = &access.authority.namespace;
            let (Some(uid), Some(gid)) = (user.to_kernel(true, uid), user.to_kernel(false, gid))
            else {
                return -errno::EINVAL;
            };
            let mode = u32::from_ne_bytes(bytes[20..24].try_into().unwrap()) as u16;
            namespace
                .semaphore_update(id, uid, gid, mode, &access, now)
                .map_or_else(ipc_error, |()| 0)
        }
        IPC_INFO | SEM_INFO => {
            let (fields, maximum) = namespace.semaphore_info(command == SEM_INFO);
            let mut bytes = [0u8; 40];
            for (position, value) in fields.into_iter().enumerate() {
                bytes[position * 4..position * 4 + 4].copy_from_slice(&value.to_ne_bytes());
            }
            task.copy_to_user(argument, &bytes)
                .map_or(-errno::EFAULT, |()| maximum as isize)
        }
        IPC_STAT | SEM_STAT | SEM_STAT_ANY => {
            let snapshot = match namespace.semaphore_snapshot(
                id,
                command != IPC_STAT,
                command == SEM_STAT_ANY,
                &access,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => return ipc_error(error),
            };
            let mut bytes = [0u8; SEM_SET_STAT_BYTES];
            encode_permission(
                &mut bytes,
                &snapshot.permission,
                snapshot.id,
                &access.authority.namespace,
            );
            for (offset, value) in [
                (48, snapshot.operation_time),
                (56, snapshot.change_time),
                (64, snapshot.count as u64),
            ] {
                bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
            }
            task.copy_to_user(argument, &bytes)
                .map_or(-errno::EFAULT, |()| {
                    if command == IPC_STAT {
                        0
                    } else {
                        snapshot.id as isize
                    }
                })
        }
        GETVAL | GETPID | GETNCNT | GETZCNT | GETALL | SETVAL | SETALL => {
            // SETVAL 检查 native int 值先于 object lookup；semnum 仅对单值命令有意义。
            let value = argument as i32;
            if command == SETVAL && !(0..=SEM_VALUE_MAX).contains(&value) {
                return -errno::ERANGE;
            }
            let set = match namespace.semaphore_set(id) {
                Ok(set) => set,
                Err(error) => return ipc_error(error),
            };
            match command {
                GETVAL | GETPID | GETNCNT | GETZCNT => {
                    if index < 0 {
                        return -errno::EINVAL;
                    }
                    set.scalar(index as usize, command, &access, &task.pid_namespace())
                        .map_or_else(ipc_error, |value| value as isize)
                }
                SETVAL => {
                    if index < 0 {
                        return -errno::EINVAL;
                    }
                    set.set_values(
                        Some(index as usize),
                        &[value as u16],
                        &access,
                        task.tgid_identity(),
                        now,
                    )
                    .map_or_else(ipc_error, |()| 0)
                }
                GETALL => {
                    let values = match set.values(&access) {
                        Ok(values) => values,
                        Err(error) => return ipc_error(error),
                    };
                    let mut bytes = Vec::new();
                    if bytes.try_reserve_exact(values.len() * 2).is_err() {
                        return -errno::ENOMEM;
                    }
                    for value in values {
                        bytes.extend_from_slice(&value.to_ne_bytes());
                    }
                    task.copy_to_user(argument, &bytes)
                        .map_or(-errno::EFAULT, |()| 0)
                }
                SETALL => {
                    let count = set.count();
                    let mut bytes = Vec::new();
                    let mut values = Vec::new();
                    if bytes.try_reserve_exact(count * 2).is_err()
                        || values.try_reserve_exact(count).is_err()
                    {
                        return -errno::ENOMEM;
                    }
                    if task
                        .copy_from_user_uninit(
                            argument,
                            &mut bytes.spare_capacity_mut()[..count * 2],
                        )
                        .is_err()
                    {
                        return -errno::EFAULT;
                    }
                    // SAFETY: user-copy 已完整初始化预留 array，长度此前为零，未公开未初始化数据。
                    unsafe {
                        bytes.set_len(count * 2);
                    }
                    for bytes in bytes.as_chunks::<2>().0 {
                        values.push(u16::from_ne_bytes(*bytes));
                    }
                    set.set_values(None, &values, &access, task.tgid_identity(), now)
                        .map_or_else(ipc_error, |()| 0)
                }
                _ => unreachable!(),
            }
        }
        _ => -errno::EINVAL,
    }
}
