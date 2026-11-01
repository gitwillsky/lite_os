use crate::{execution::task::current_task, syscall::errno, system};

const UTS_FIELD_BYTES: usize = 65;
const UTS_FIELD_COUNT: usize = 6;
const UTS_BYTES: usize = UTS_FIELD_BYTES * UTS_FIELD_COUNT;

/// 按 Linux v7.1 `new_utsname` ABI 返回immutable build fields 与 calling Thread 的实际 UTS namespace 字段。
///
/// # Parameters
///
/// - `address`: 用户态 390-byte `struct new_utsname` 输出地址。
///
/// # Returns
///
/// 成功返回零；用户地址不可写返回 `-EFAULT`。
pub(crate) fn sys_uname(address: usize) -> isize {
    let mut bytes = [0u8; UTS_BYTES];
    for (index, field) in system::identity().into_iter().enumerate() {
        if index == 1 || index == 5 {
            continue;
        }
        let field = field.as_bytes();
        assert!(
            field.len() < UTS_FIELD_BYTES,
            "system identity exceeds utsname ABI"
        );
        let offset = index * UTS_FIELD_BYTES;
        bytes[offset..offset + field.len()].copy_from_slice(field);
    }
    let task = current_task().expect("uname requires a current task");
    let names = task.uts_namespace().snapshot();
    bytes[UTS_FIELD_BYTES..2 * UTS_FIELD_BYTES].copy_from_slice(&names.hostname);
    bytes[5 * UTS_FIELD_BYTES..6 * UTS_FIELD_BYTES].copy_from_slice(&names.domainname);
    if task.copy_to_user(address, &bytes).is_err() {
        -errno::EFAULT
    } else {
        0
    }
}

/// 标准 sethostname/setdomainname；权限先于 length/user-copy，目标 namespace owner 原子提交。
pub(crate) fn sys_set_uts_name(hostname: bool, address: usize, length: usize) -> isize {
    let task = current_task().expect("UTS mutation requires current Thread");
    let namespace = task.uts_namespace();
    let authority = task.namespace_authority();
    if !authority.capable(
        &namespace.user_namespace(),
        syscall_abi::capability::Capability::SYS_ADMIN,
    ) {
        return -errno::EPERM;
    }
    let length = length as i32;
    if !(0..=64).contains(&length) {
        return -errno::EINVAL;
    }
    let mut bytes = [0; 64];
    if length != 0
        && task
            .copy_from_user(address, &mut bytes[..length as usize])
            .is_err()
    {
        return -errno::EFAULT;
    }
    namespace
        .set_name(hostname, &bytes[..length as usize], &authority)
        .map_or_else(
            |error| match error {
                crate::namespace::NamespaceError::Permission => -errno::EPERM,
                _ => -errno::EINVAL,
            },
            |_| 0,
        )
}
