//! Linux System V IPC raw ABI、user-copy 与 errno；对象/等待/权限由 actual IPC namespace 拥有。
mod semaphore;
mod shared_memory;
use crate::{
    ipc::{IpcError, MessageLimit, MessageQueueWait, MessageReceive, PreparedMessage},
    sync::WaitResult,
    task::current_task,
};
use alloc::vec::Vec;
pub(super) use semaphore::{sys_semctl, sys_semget, sys_semop, sys_semtimedop};
pub(super) use shared_memory::{sys_shmat, sys_shmctl, sys_shmdt, sys_shmget};
use syscall_abi::{errno, ipc::*};

pub(super) fn ipc_error(error: IpcError) -> isize {
    -match error {
        IpcError::Invalid => errno::EINVAL,
        IpcError::Permission => errno::EPERM,
        IpcError::Access => errno::EACCES,
        IpcError::NoMemory => errno::ENOMEM,
        IpcError::NoSpace => errno::ENOSPC,
        IpcError::Exists => errno::EEXIST,
        IpcError::NotFound => errno::ENOENT,
        IpcError::Removed => errno::EIDRM,
        IpcError::TooBig => errno::E2BIG,
        IpcError::Again => errno::EAGAIN,
        IpcError::Range => errno::ERANGE,
        IpcError::Index => errno::EFBIG,
    }
}
pub(super) fn sys_msgget(key: i32, flags: u32) -> isize {
    let task = current_task().expect("msgget requires current Thread");
    task.ipc_namespace()
        .message_get(
            key,
            flags,
            &task.ipc_access(),
            crate::timer::get_realtime_ns() / 1_000_000_000,
        )
        .map_or_else(ipc_error, |id| id as isize)
}
pub(super) fn sys_msgsnd(id: i32, address: usize, size: usize, flags: u32) -> isize {
    let task = current_task().expect("msgsnd requires current Thread");
    let mut kind = [0u8; 8];
    if task.copy_from_user(address, &mut kind).is_err() {
        return -errno::EFAULT;
    }
    let namespace = task.ipc_namespace();
    let kind = i64::from_ne_bytes(kind);
    if id < 0 || kind <= 0 || size > namespace.message_limit(MessageLimit::Maximum) as usize {
        return -errno::EINVAL;
    }
    let Some(payload) = address.checked_add(8) else {
        return -errno::EFAULT;
    };
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(size).is_err() {
        return -errno::ENOMEM;
    }
    if task
        .copy_from_user_uninit(payload, &mut bytes.spare_capacity_mut()[..size])
        .is_err()
    {
        return -errno::EFAULT;
    }
    // SAFETY: 此 Vec 独占刚预留的 storage；上一行 user-copy 完整初始化 size 字节后才公布长度。
    unsafe {
        bytes.set_len(size);
    }
    let mut message = match PreparedMessage::new(kind, bytes) {
        Ok(message) => message,
        Err(error) => return ipc_error(error),
    };
    let queue = match namespace.message_queue(id) {
        Ok(queue) => queue,
        Err(error) => return ipc_error(error),
    };
    let access = task.ipc_access();
    loop {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        match queue.send(
            &mut message,
            &access,
            task.tgid_identity().clone(),
            now,
            None,
        ) {
            Ok(true) => return 0,
            Err(error) => return ipc_error(error),
            Ok(false) => {}
        }
        if flags & IPC_NOWAIT != 0 {
            return -errno::EAGAIN;
        }
        let mut waiter = match MessageQueueWait::new(&queue) {
            Ok(waiter) => waiter,
            Err(error) => return ipc_error(error),
        };
        match queue.send(
            &mut message,
            &access,
            task.tgid_identity().clone(),
            now,
            Some(&mut waiter),
        ) {
            Ok(true) => return 0,
            Err(error) => return ipc_error(error),
            Ok(false) => {}
        }
        if waiter.wait() == WaitResult::Interrupted {
            return super::INTERNAL_RESTART_NO_HANDLER;
        }
    }
}
pub(super) fn sys_msgrcv(id: i32, address: usize, size: usize, kind: i64, flags: u32) -> isize {
    if id < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("msgrcv requires current Thread");
    let namespace = task.ipc_namespace();
    let request = match MessageReceive::new(
        kind,
        flags,
        size,
        namespace.message_limit(MessageLimit::Maximum) as usize,
    ) {
        Ok(request) => request,
        Err(error) => return ipc_error(error),
    };
    // Linux prepare_copy 从 buf（含 mtype 的起始地址）预读 min(bufsz,msgmax)，早于 ID/DAC 查找。
    // 保留这份实际 storage 至 completion：写专用或不可读映射必须在 queue lookup 前返回 EFAULT。
    let copy_buffer = if let Some(capacity) = request.copy_capacity {
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(capacity).is_err() {
            return -errno::ENOMEM;
        }
        if task
            .copy_from_user_uninit(address, &mut bytes.spare_capacity_mut()[..capacity])
            .is_err()
        {
            return -errno::EFAULT;
        }
        // SAFETY: 完整 user-copy 已初始化独占 Vec 的 capacity 字节，尚未发布其长度。
        unsafe {
            bytes.set_len(capacity);
        }
        Some(bytes)
    } else {
        None
    };
    let queue = match namespace.message_queue(id) {
        Ok(queue) => queue,
        Err(error) => return ipc_error(error),
    };
    let access = task.ipc_access();
    let message = loop {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        match queue.receive(&request, &access, task.tgid_identity().clone(), now, None) {
            Ok(Some(message)) => break message,
            Err(error) => return ipc_error(error),
            Ok(None) => {}
        }
        if flags & IPC_NOWAIT != 0 {
            return -errno::ENOMSG;
        }
        let mut waiter = match MessageQueueWait::receiver(&queue, request, task.tid_identity()) {
            Ok(waiter) => waiter,
            Err(error) => return ipc_error(error),
        };
        match queue.receive(
            &request,
            &access,
            task.tgid_identity().clone(),
            now,
            Some(&mut waiter),
        ) {
            Ok(Some(message)) => break message,
            Err(error) => return ipc_error(error),
            Ok(None) => {}
        }
        let waited = waiter.wait();
        if let Some(received) = waiter.finish_wait() {
            match received {
                Ok(message) => break message,
                Err(error) => return ipc_error(error),
            }
        }
        if waited == WaitResult::Interrupted {
            return super::INTERNAL_RESTART_NO_HANDLER;
        }
    };
    let count = size.min(message.bytes.len());
    let Some(payload) = address.checked_add(8) else {
        return -errno::EFAULT;
    };
    // Linux dequeue 先于 do_msg_fill；用户目标 fault 消费已出队消息，MSG_COPY 则始终不消费。
    if task
        .copy_to_user(address, &message.kind.to_ne_bytes())
        .is_err()
        || task.copy_to_user(payload, &message.bytes[..count]).is_err()
    {
        return -errno::EFAULT;
    }
    drop(copy_buffer);
    count as isize
}
pub(super) fn sys_msgctl(id: i32, command: i32, address: usize) -> isize {
    if id < 0 || command < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("msgctl requires current Thread");
    let namespace = task.ipc_namespace();
    let access = task.ipc_access();
    match command {
        IPC_RMID => namespace
            .message_remove(id, &access)
            .map_or_else(ipc_error, |()| 0),
        IPC_SET => {
            let mut bytes = [0u8; MSG_QUEUE_STAT_BYTES];
            if task.copy_from_user(address, &mut bytes).is_err() {
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
            let limit = u64::from_ne_bytes(bytes[88..96].try_into().unwrap()) as usize;
            namespace
                .message_update(
                    id,
                    uid,
                    gid,
                    mode,
                    limit,
                    &access,
                    crate::timer::get_realtime_ns() / 1_000_000_000,
                )
                .map_or_else(ipc_error, |()| 0)
        }
        IPC_INFO | MSG_INFO => {
            let (fields, segments, maximum) =
                match namespace.message_info(command == MSG_INFO, &access) {
                    Ok(info) => info,
                    Err(error) => return ipc_error(error),
                };
            let mut bytes = [0u8; 32];
            for (index, field) in fields.into_iter().enumerate() {
                bytes[index * 4..index * 4 + 4].copy_from_slice(&field.to_ne_bytes());
            }
            bytes[28..30].copy_from_slice(&segments.to_ne_bytes());
            task.copy_to_user(address, &bytes)
                .map_or(-errno::EFAULT, |()| maximum as isize)
        }
        IPC_STAT | MSG_STAT | MSG_STAT_ANY => {
            let snapshot = match namespace.message_snapshot(
                id,
                command != IPC_STAT,
                command == MSG_STAT_ANY,
                &access,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => return ipc_error(error),
            };
            let mut bytes = [0u8; MSG_QUEUE_STAT_BYTES];
            encode_permission(
                &mut bytes,
                &snapshot.permission,
                snapshot.id,
                &access.authority.namespace,
            );
            for (offset, value) in [
                (48, snapshot.send_time),
                (56, snapshot.receive_time),
                (64, snapshot.change_time),
                (72, snapshot.bytes as u64),
                (80, snapshot.messages as u64),
                (88, snapshot.limit as u64),
            ] {
                bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
            }
            let view = task.pid_namespace();
            for (offset, pid) in [(96, snapshot.last_sender), (100, snapshot.last_receiver)] {
                bytes[offset..offset + 4].copy_from_slice(
                    &pid.map_or(0, |pid| pid.number_in(&view) as i32)
                        .to_ne_bytes(),
                );
            }
            task.copy_to_user(address, &bytes)
                .map_or(-errno::EFAULT, |()| {
                    if command == IPC_STAT {
                        0
                    } else {
                        snapshot.id as isize
                    }
                })
        }
        _ => -errno::EINVAL,
    }
}

// 标准 IPC64 stat codec 共用同一权限布局；输入仅为本次 actual-object 的只读投影。
fn encode_permission(
    bytes: &mut [u8],
    permission: &crate::ipc::IpcPermission,
    id: i32,
    user: &crate::namespace::UserNamespace,
) {
    bytes[..4].copy_from_slice(&permission.key.to_ne_bytes());
    for (offset, uid, value) in [
        (4, true, permission.uid),
        (8, false, permission.gid),
        (12, true, permission.creator_uid),
        (16, false, permission.creator_gid),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&user.display_id(uid, value).to_ne_bytes());
    }
    bytes[20..24].copy_from_slice(&u32::from(permission.mode).to_ne_bytes());
    bytes[24..26].copy_from_slice(&((id as u32 >> 15) as u16).to_ne_bytes());
}
