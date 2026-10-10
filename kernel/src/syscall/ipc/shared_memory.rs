//! Linux native64 shm UAPI；对象、挂接计数与映射生命周期不在 codec 中维护。
use super::{encode_permission, ipc_error};
use crate::{
    ipc::IpcError,
    memory::{MapPermission, MemoryError, PAGE_SIZE},
    task::current_task,
};
use syscall_abi::{errno, ipc::*};

pub(in crate::syscall) fn sys_shmget(key: i32, size: usize, flags: u32) -> isize {
    let task = current_task().expect("shmget requires current Thread");
    task.create_shared_memory(key, size, flags)
        .map_or_else(ipc_error, |id| id as isize)
}
pub(in crate::syscall) fn sys_shmat(id: i32, mut address: usize, flags: u32) -> isize {
    if id < 0 {
        return -errno::EINVAL;
    }
    // 当前两种 ISA 的 SHMLBA 均为 PAGE_SIZE；未知 shmflg bit 与 Linux 一样忽略。
    if address != 0 && !address.is_multiple_of(PAGE_SIZE) {
        if flags & SHM_RND == 0 {
            return -errno::EINVAL;
        }
        address &= !(PAGE_SIZE - 1);
    }
    let remap = flags & SHM_REMAP != 0;
    if remap && address == 0 {
        return -errno::EINVAL;
    }
    let may_write = flags & SHM_RDONLY == 0;
    let mut permission = MapPermission::U | MapPermission::R;
    let mut requested = 4;
    if may_write {
        permission |= MapPermission::W;
        requested |= 2;
    }
    if flags & SHM_EXEC != 0 {
        permission |= MapPermission::X;
        requested |= 1;
    }
    let task = current_task().expect("shmat requires current Thread");
    let namespace = task.ipc_namespace();
    let reservation =
        match namespace.shared_memory_prepare_attach(id, requested, &task.ipc_access()) {
            Ok(reservation) => reservation,
            Err(error) => return ipc_error(error),
        };
    task.attach_shared_memory(reservation, address, permission, may_write, remap)
        .map_or_else(
            |error| -match error {
                MemoryError::InvalidRange | MemoryError::AddressInUse => errno::EINVAL,
                MemoryError::PermissionDenied => errno::EACCES,
                _ => errno::ENOMEM,
            },
            |address| address as isize,
        )
}
pub(in crate::syscall) fn sys_shmdt(address: usize) -> isize {
    current_task()
        .expect("shmdt requires current Thread")
        .detach_shared_memory(address)
        .map_or_else(
            |error| -match error {
                MemoryError::OutOfMemory => errno::ENOMEM,
                _ => errno::EINVAL,
            },
            |()| 0,
        )
}
pub(in crate::syscall) fn sys_shmctl(id: i32, command: i32, address: usize) -> isize {
    if id < 0 || command < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("shmctl requires current Thread");
    let namespace = task.ipc_namespace();
    let access = task.ipc_access();
    match command {
        SHM_LOCK | SHM_UNLOCK => task
            .lock_shared_memory(id, command == SHM_LOCK)
            .map_or_else(ipc_error, |()| 0),
        IPC_RMID => namespace
            .shared_memory_remove(id, &access)
            .map_or_else(ipc_error, |()| 0),
        IPC_SET => {
            let mut bytes = [0; SHM_SEGMENT_STAT_BYTES];
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
            namespace
                .shared_memory_update(
                    id,
                    uid,
                    gid,
                    u32::from_ne_bytes(bytes[20..24].try_into().unwrap()) as u16,
                    &access,
                    crate::timer::get_realtime_ns() / 1_000_000_000,
                )
                .map_or_else(ipc_error, |()| 0)
        }
        IPC_INFO => {
            let (fields, highest) = namespace.shared_memory_limits();
            let mut bytes = [0; 72];
            for (index, field) in fields.into_iter().enumerate() {
                bytes[index * 8..index * 8 + 8].copy_from_slice(&field.to_ne_bytes());
            }
            task.copy_to_user(address, &bytes)
                .map_or(-errno::EFAULT, |()| highest as isize)
        }
        SHM_INFO => {
            let (used, fields, highest) = namespace.shared_memory_info();
            let mut bytes = [0; 48];
            bytes[..4].copy_from_slice(&used.to_ne_bytes());
            for (index, field) in fields.into_iter().enumerate() {
                bytes[8 + index * 8..16 + index * 8].copy_from_slice(&field.to_ne_bytes());
            }
            task.copy_to_user(address, &bytes)
                .map_or(-errno::EFAULT, |()| highest as isize)
        }
        IPC_STAT | SHM_STAT | SHM_STAT_ANY => {
            let snapshot = match namespace.shared_memory_snapshot(
                id,
                command != IPC_STAT,
                command == SHM_STAT_ANY,
                &access,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => return ipc_error(error),
            };
            let mut bytes = [0; SHM_SEGMENT_STAT_BYTES];
            encode_permission(
                &mut bytes,
                &snapshot.permission,
                snapshot.id,
                &access.authority.namespace,
            );
            for (offset, value) in [
                (48, snapshot.size as u64),
                (56, snapshot.attach_time),
                (64, snapshot.detach_time),
                (72, snapshot.change_time),
                (88, snapshot.attaches as u64),
            ] {
                bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
            }
            let view = task.pid_namespace();
            bytes[80..84]
                .copy_from_slice(&(snapshot.creator.number_in(&view) as i32).to_ne_bytes());
            bytes[84..88].copy_from_slice(
                &snapshot
                    .last_operator
                    .map_or(0, |pid| pid.number_in(&view) as i32)
                    .to_ne_bytes(),
            );
            task.copy_to_user(address, &bytes)
                .map_or(-errno::EFAULT, |()| {
                    if command == IPC_STAT {
                        0
                    } else {
                        snapshot.id as isize
                    }
                })
        }
        _ => ipc_error(IpcError::Invalid),
    }
}
