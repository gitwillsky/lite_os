use crate::{syscall::errno, system, task::current_task};

const UTS_FIELD_BYTES: usize = 65;
const UTS_FIELD_COUNT: usize = 6;
const UTS_BYTES: usize = UTS_FIELD_BYTES * UTS_FIELD_COUNT;

/// @description 按 Linux v7.1 `new_utsname` ABI 返回不可变 system/build identity。
///
/// @param address 用户态 390-byte `struct new_utsname` 输出地址。
/// @return 成功返回零；用户地址不可写返回 `-EFAULT`。
pub(crate) fn sys_uname(address: usize) -> isize {
    let mut bytes = [0u8; UTS_BYTES];
    for (index, field) in system::identity().into_iter().enumerate() {
        let field = field.as_bytes();
        assert!(
            field.len() < UTS_FIELD_BYTES,
            "system identity exceeds utsname ABI"
        );
        let offset = index * UTS_FIELD_BYTES;
        bytes[offset..offset + field.len()].copy_from_slice(field);
    }
    let task = current_task().expect("uname requires a current task");
    if task.copy_to_user(address, &bytes).is_err() {
        -errno::EFAULT
    } else {
        0
    }
}
