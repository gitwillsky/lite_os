use alloc::vec::Vec;

use crate::{
    syscall::errno,
    task::{CredentialUpdateError, current_task},
};

/// 返回calling Thread 的 real/effective UID/GID。
pub(crate) fn sys_get_id(uid: bool, effective: bool) -> isize {
    current_task()
        .expect("identity syscall requires current task")
        .visible_credential_id(uid, effective) as isize
}

/// 按 Linux setuid/setgid 规则更新 calling Thread credentials。
pub(crate) fn sys_set_id(uid: bool, value: u32) -> isize {
    current_task()
        .expect("set identity requires current task")
        .set_credential_id(uid, value)
        .map_or_else(credential_error, |()| 0)
}

/// Linux setfsuid/setfsgid 没有 errno；返回calling Thread旧 filesystem ID。
pub(crate) fn sys_set_fs_id(uid: bool, value: u32) -> isize {
    current_task()
        .expect("setfsid requires current Thread")
        .set_filesystem_id(uid, value) as isize
}

/// 向三个用户指针写出 real/effective/saved UID/GID。
pub(crate) fn sys_get_res_ids(uid: bool, pointers: [usize; 3]) -> isize {
    let task = current_task().expect("getresid requires current task");
    let values = task.visible_credential_res_ids(uid);
    for (pointer, value) in pointers.into_iter().zip(values) {
        if task.copy_to_user(pointer, &value.to_ne_bytes()).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}

/// 原子应用 setresuid/setresgid 三元组。
pub(crate) fn sys_set_res_ids(uid: bool, values: [u32; 3]) -> isize {
    current_task()
        .expect("setresid requires current task")
        .set_credential_res_ids(uid, values)
        .map_or_else(credential_error, |()| 0)
}

/// 实现 Linux getgroups size query 与 group array copyout。
pub(crate) fn sys_getgroups(size: usize, list: usize) -> isize {
    let size = size as i32;
    if size < 0 {
        return -errno::EINVAL;
    }
    let size = size as usize;
    let task = current_task().expect("getgroups requires current task");
    let snapshot = task.supplementary_groups();
    let groups = snapshot.as_deref().map_or(&[][..], Vec::as_slice);
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
    let namespace = task.user_namespace();
    for group in groups {
        encoded.extend_from_slice(&namespace.display_id(false, *group).to_ne_bytes());
    }
    if task.copy_to_user(list, &encoded).is_err() {
        -errno::EFAULT
    } else {
        encoded.len() as isize / 4
    }
}

/// 按 CAP_SETGID 原子替换 supplementary groups。
pub(crate) fn sys_setgroups(size: usize, list: usize) -> isize {
    const NGROUPS_MAX: usize = 65_536;
    let task = current_task().expect("setgroups requires current task");
    if !task.has_capability(syscall_abi::capability::Capability::SETGID)
        || !task.user_namespace().may_setgroups()
    {
        return -errno::EPERM;
    }
    let size = size as u32 as usize;
    if size > NGROUPS_MAX {
        return -errno::EINVAL;
    }
    let mut groups = Vec::new();
    if groups.try_reserve_exact(size).is_err() {
        return -errno::ENOMEM;
    }
    let namespace = task.user_namespace();
    // Linux groups_from_user 按元素检查；首个非法 gid 必须先于后续页的 EFAULT 返回。
    for index in 0..size {
        let Some(address) = list.checked_add(index * 4) else {
            return -errno::EFAULT;
        };
        let mut encoded = [0; 4];
        if task.copy_from_user(address, &mut encoded).is_err() {
            return -errno::EFAULT;
        }
        let group = u32::from_ne_bytes(encoded);
        if namespace.to_kernel(false, group).is_none() {
            return -errno::EINVAL;
        }
        groups.push(group);
    }
    task.set_supplementary_groups(groups)
        .map_or_else(credential_error, |()| 0)
}

/// 替换 calling Thread 共享 fs context 的 umask，并返回旧的低 9-bit mask。
pub(crate) fn sys_umask(mask: u32) -> isize {
    current_task()
        .expect("umask requires current task")
        .replace_umask(mask) as isize
}

fn credential_error(error: CredentialUpdateError) -> isize {
    -match error {
        CredentialUpdateError::Permission => errno::EPERM,
        CredentialUpdateError::Invalid => errno::EINVAL,
        CredentialUpdateError::OutOfMemory => errno::ENOMEM,
    }
}
