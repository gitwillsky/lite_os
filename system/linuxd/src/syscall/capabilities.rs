use crate::{
    syscall::errno,
    task::{self, CapabilityError, current_task},
};
use syscall_abi::capability::{CapabilitySets, VERSION_3, data_size};

/// version 先于 PID/data 读取；未知版本只更新首个 u32，不访问不可读 PID。
fn version_size(header: usize) -> Result<usize, isize> {
    let caller = current_task().expect("capability syscall requires current Thread");
    let mut version = [0; 4];
    caller
        .copy_from_user(header, &mut version)
        .map_err(|_| -errno::EFAULT)?;
    if let Some(size) = data_size(u32::from_ne_bytes(version)) {
        return Ok(size);
    }
    caller
        .copy_to_user(header, &VERSION_3.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)?;
    Err(-errno::EINVAL)
}
fn header_pid(header: usize) -> Result<i32, isize> {
    let address = header.checked_add(4).ok_or(-errno::EFAULT)?;
    let mut pid = [0; 4];
    current_task()
        .unwrap()
        .copy_from_user(address, &mut pid)
        .map_err(|_| -errno::EFAULT)?;
    Ok(i32::from_ne_bytes(pid))
}
/// 固定 Linux capget；NULL data 的版本发现不读取 PID，unknown version 写回后返回成功。
pub(crate) fn sys_capget(header: usize, data: usize) -> isize {
    let size = match version_size(header) {
        Ok(size) => size,
        Err(error) if data == 0 && error == -errno::EINVAL => return 0,
        Err(error) => return error,
    };
    if data == 0 {
        return 0;
    }
    let pid = match header_pid(header) {
        Ok(pid) if pid >= 0 => pid as usize,
        Ok(_) => return -errno::EINVAL,
        Err(error) => return error,
    };
    let Some(sets) = task::capability_sets(pid) else {
        return -errno::ESRCH;
    };
    current_task()
        .unwrap()
        .copy_to_user(data, &sets.encode()[..size])
        .map_or(-errno::EFAULT, |()| 0)
}
/// capset 只修改 calling Thread；target 权限先于 data copy，统一旧/新版本 codec。
pub(crate) fn sys_capset(header: usize, data: usize) -> isize {
    let size = match version_size(header) {
        Ok(size) => size,
        Err(error) => return error,
    };
    let pid = match header_pid(header) {
        Ok(pid) => pid,
        Err(error) => return error,
    };
    let caller = current_task().unwrap();
    if pid != 0 && pid != caller.visible_tid() as i32 {
        return -errno::EPERM;
    }
    let mut bytes = [0; 24];
    if caller.copy_from_user(data, &mut bytes[..size]).is_err() {
        return -errno::EFAULT;
    }
    caller
        .set_capabilities(
            CapabilitySets::decode(&bytes[..size]).expect("validated capability layout"),
        )
        .map_or_else(capability_error, |()| 0)
}
pub(super) fn capability_error(error: CapabilityError) -> isize {
    -match error {
        CapabilityError::Permission => errno::EPERM,
        CapabilityError::Invalid => errno::EINVAL,
    }
}
