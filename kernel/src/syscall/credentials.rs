use alloc::vec::Vec;

use crate::{syscall::errno, task::current_task};

/// @description 返回当前 Process 的 real/effective UID/GID。
pub(crate) fn sys_get_id(uid: bool, effective: bool) -> isize {
    current_task()
        .expect("identity syscall requires current task")
        .credential_id(uid, effective) as isize
}

/// @description 按 Linux setuid/setgid 规则更新 Process credentials。
pub(crate) fn sys_set_id(uid: bool, value: u32) -> isize {
    current_task()
        .expect("set identity requires current task")
        .set_credential_id(uid, value)
        .map_or(-errno::EPERM, |()| 0)
}

/// @description 向三个用户指针写出 real/effective/saved UID/GID。
pub(crate) fn sys_get_res_ids(uid: bool, pointers: [usize; 3]) -> isize {
    let task = current_task().expect("getresid requires current task");
    let values = task.credential_res_ids(uid);
    for (pointer, value) in pointers.into_iter().zip(values) {
        if task.copy_to_user(pointer, &value.to_ne_bytes()).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}

/// @description 原子应用 setresuid/setresgid 三元组。
pub(crate) fn sys_set_res_ids(uid: bool, values: [u32; 3]) -> isize {
    current_task()
        .expect("setresid requires current task")
        .set_credential_res_ids(uid, values)
        .map_or(-errno::EPERM, |()| 0)
}

/// @description 实现 Linux getgroups size query 与 group array copyout。
pub(crate) fn sys_getgroups(size: usize, list: usize) -> isize {
    let task = current_task().expect("getgroups requires current task");
    let groups = task.supplementary_groups();
    if size == 0 {
        return groups.len() as isize;
    }
    if size < groups.len() {
        return -errno::EINVAL;
    }
    if groups.is_empty() {
        return 0;
    }
    let mut encoded = Vec::new();
    if encoded
        .try_reserve_exact(groups.len().saturating_mul(4))
        .is_err()
    {
        return -errno::ENOMEM;
    }
    for group in groups {
        encoded.extend_from_slice(&group.to_ne_bytes());
    }
    if task.copy_to_user(list, &encoded).is_err() {
        -errno::EFAULT
    } else {
        encoded.len() as isize / 4
    }
}

/// @description 仅允许 effective root 原子替换 supplementary groups。
pub(crate) fn sys_setgroups(size: usize, list: usize) -> isize {
    const NGROUPS_MAX: usize = 65_536;
    if size > NGROUPS_MAX {
        return -errno::EINVAL;
    }
    let task = current_task().expect("setgroups requires current task");
    let Some(bytes) = size.checked_mul(4) else {
        return -errno::EINVAL;
    };
    let mut encoded = Vec::new();
    if encoded.try_reserve_exact(bytes).is_err() {
        return -errno::ENOMEM;
    }
    encoded.resize(bytes, 0);
    if !encoded.is_empty() && task.copy_from_user(list, &mut encoded).is_err() {
        return -errno::EFAULT;
    }
    let groups = encoded
        .chunks_exact(4)
        .map(|value| u32::from_ne_bytes(value.try_into().unwrap()))
        .collect();
    task.set_supplementary_groups(groups)
        .map_or(-errno::EPERM, |()| 0)
}

/// @description 替换 Process umask 并返回旧的低 9-bit mask。
pub(crate) fn sys_umask(mask: u32) -> isize {
    current_task()
        .expect("umask requires current task")
        .replace_umask(mask) as isize
}
