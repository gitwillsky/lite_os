//! 标准 PIDFS ioctl 的唯一 extensible UAPI codec。
use super::super::device::{DeviceError, IoctlCall};
use super::PidFdTarget;

/// PIDFS 是标准 ioctl UAPI owner；Task binding 只选择实际 association/namespace 和授权。
pub(super) fn ioctl(target: &dyn PidFdTarget, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    use crate::namespace::{NamespaceSelector as Selector, NamespaceType as Kind};
    use syscall_abi::errno;
    if call.request == 0x8008_7601 {
        if call.argument == 0 {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        call.user
            .write(
                call.argument,
                &((target.identity().key() as u64 >> 32) as u32).to_ne_bytes(),
            )
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        return Ok(0);
    }
    if call.request & 0xc000_ffff == 0xc000_ff0b && (call.request >> 16 & 0x3fff) >= 64 {
        return pidfd_information(target, call);
    }
    let selector = match call.request {
        0xff01 => None,
        0xff04 => Some(Selector::Namespace(Kind::Net)),
        0xff02 => Some(Selector::Namespace(Kind::Ipc)),
        0xff03 => Some(Selector::Namespace(Kind::Mount)),
        0xff05 => Some(Selector::Namespace(Kind::Pid)),
        0xff06 => Some(Selector::PidForChildren),
        0xff07 => Some(Selector::Namespace(Kind::Time)),
        0xff08 => Some(Selector::TimeForChildren),
        0xff09 => Some(Selector::Namespace(Kind::User)),
        0xff0a => Some(Selector::Namespace(Kind::Uts)),
        _ => return Err(DeviceError::Errno(errno::ENOTTY)),
    };
    let namespace = target.namespace(selector, call.argument != 0)?;
    call.user.install_namespace(namespace)
}

/// 原生 extensible GET_INFO：只读 input mask，短结构裁掉无法容纳的 flag，未知输出尾部清零。
/// 历史退出字段从唯一 PIDFS retirement record 读取，不维持 Task 的 alive cache。
fn pidfd_information(target: &dyn PidFdTarget, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    use syscall_abi::errno;
    if call.argument == 0 {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    let mut mask = [0u8; 8];
    call.user
        .read(call.argument, &mut mask)
        .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
    let requested = u64::from_ne_bytes(mask);
    let view = call.user.pid_namespace();
    if target.identity().number_in(&view) == 0 {
        return Err(DeviceError::Errno(errno::EREMOTE));
    }
    let mut bytes = [0u8; 88];
    let mut provided = 0u64;
    if requested & 8 != 0
        && let Some(status) = target.source().exit_status()
    {
        provided |= 8;
        bytes[60..64].copy_from_slice(&status.to_ne_bytes());
    }
    if let Some(identifiers) = target.information(&view) {
        for (field, value) in bytes[16..60]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(identifiers)
        {
            field.copy_from_slice(&value.to_ne_bytes());
        }
        provided |= 1 | 2;
    } else if requested & 8 == 0 {
        return Err(DeviceError::Errno(errno::ESRCH));
    }
    if requested & 32 != 0 {
        provided |= 32;
        bytes[80..88].copy_from_slice(&(1u64 | 2 | 8 | 32).to_ne_bytes());
    }
    let size = call.request >> 16 & 0x3fff;
    if size < 88 {
        provided &= !32;
    }
    bytes[..8].copy_from_slice(&provided.to_ne_bytes());
    call.user
        .write(call.argument, &bytes[..size.min(bytes.len())])
        .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
    let mut position = bytes.len();
    let zeros = [0u8; 64];
    while position < size {
        let count = (size - position).min(zeros.len());
        let address = call
            .argument
            .checked_add(position)
            .ok_or(DeviceError::Errno(errno::EFAULT))?;
        call.user
            .write(address, &zeros[..count])
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        position += count;
    }
    Ok(0)
}
