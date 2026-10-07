//! Linux evdev ioctl UAPI 子集：query、clock、exclusive grab 与 revoke。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{InputError, InputFile, InputString};
use crate::fs::device::{DeviceError, IoctlCall, UserMemory};

const IOC_WRITE: usize = 1;
const IOC_READ: usize = 2;
const INPUT_IOCTL_TYPE: usize = b'E' as usize;
const EV_VERSION: i32 = 0x010001;

const fn input_ioc(direction: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | INPUT_IOCTL_TYPE << 8 | number
}

const EVIOCGVERSION: usize = input_ioc(IOC_READ, 0x01, 4);
const EVIOCGID: usize = input_ioc(IOC_READ, 0x02, 8);
const EVIOCGRAB: usize = input_ioc(IOC_WRITE, 0x90, 4);
const EVIOCREVOKE: usize = input_ioc(IOC_WRITE, 0x91, 4);
const EVIOCSCLOCKID: usize = input_ioc(IOC_WRITE, 0xa0, 4);

pub(super) fn input_error(error: InputError) -> DeviceError {
    DeviceError::Errno(match error {
        InputError::OutOfMemory => errno::ENOMEM,
        InputError::Busy => errno::EBUSY,
        InputError::Invalid => errno::EINVAL,
        InputError::Revoked => errno::ENODEV,
    })
}

pub(super) fn fault(_: crate::fs::device::UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

fn copy_out(user: &dyn UserMemory, argument: usize, bytes: &[u8]) -> Result<(), DeviceError> {
    if bytes.is_empty() {
        return Ok(());
    }
    user.write(argument, bytes).map_err(fault)
}

fn copy_in_i32(user: &dyn UserMemory, argument: usize) -> Result<i32, DeviceError> {
    let mut bytes = [0u8; 4];
    user.read(argument, &mut bytes).map_err(fault)?;
    Ok(i32::from_ne_bytes(bytes))
}

fn copy_variable(file: &InputFile, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    let request = call.request;
    let direction = request >> 30 & 0x3;
    let size = request >> 16 & 0x3fff;
    let kind = request >> 8 & 0xff;
    let number = request & 0xff;
    if direction != IOC_READ || kind != INPUT_IOCTL_TYPE {
        return Err(DeviceError::Errno(errno::ENOTTY));
    }

    let mut bytes = [0u8; 129];
    let output_length = size.min(bytes.len());
    let count = match number {
        0x06 => file.copy_string(InputString::Name, &mut bytes[..output_length]),
        0x07 => file.copy_string(InputString::PhysicalPath, &mut bytes[..output_length]),
        0x08 => file.copy_string(InputString::Serial, &mut bytes[..output_length]),
        0x09 => file
            .copy_bitmap(None, &mut bytes[..output_length])
            .map_err(input_error)?,
        0x18 => {
            let output = &mut bytes[..output_length];
            // EVIOCGKEY 读取 key state 会结束 SYN_DROPPED 重同步；先证明输出可写，避免状态被
            // 消费而用户未收到。
            if !output.is_empty() {
                call.user
                    .validate_write(call.argument, output.len().min(96))
                    .map_err(fault)?;
            }
            file.copy_key_state(output)
        }
        0x20..=0x3f => file
            .copy_bitmap(Some((number & 0x1f) as u16), &mut bytes[..output_length])
            .map_err(input_error)?,
        0x40..=0x7f => {
            let info = file
                .absolute_info((number & 0x3f) as u16)
                .map_err(input_error)?;
            for (offset, value) in [
                info.value,
                info.minimum,
                info.maximum,
                info.fuzz,
                info.flat,
                info.resolution,
            ]
            .into_iter()
            .enumerate()
            {
                bytes[offset * 4..offset * 4 + 4].copy_from_slice(&value.to_ne_bytes());
            }
            let count = size.min(24);
            copy_out(call.user, call.argument, &bytes[..count])?;
            return Ok(0);
        }
        _ => return Err(DeviceError::Errno(errno::ENOTTY)),
    };
    if let Err(error) = copy_out(call.user, call.argument, &bytes[..count]) {
        if number == 0x18 {
            file.mark_sync_lost();
        }
        return Err(error);
    }
    Ok(count as isize)
}

/// 分发 Linux evdev query、clock 与 exclusive-grab ioctl 子集。
///
/// # Returns
///
/// fixed ioctl 返回零；variable query 返回复制 byte count。
///
/// # Errors
///
/// 已撤销返回 `ENODEV`；用户地址、参数或 request 错误返回对应 errno。
pub(super) fn ioctl(file: &Arc<InputFile>, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    if file.is_revoked() {
        return Err(DeviceError::Errno(errno::ENODEV));
    }
    match call.request {
        EVIOCGVERSION => copy_out(call.user, call.argument, &EV_VERSION.to_ne_bytes()).map(|()| 0),
        EVIOCGID => {
            let id = file.id();
            let mut bytes = [0u8; 8];
            bytes[0..2].copy_from_slice(&id.bustype.to_ne_bytes());
            bytes[2..4].copy_from_slice(&id.vendor.to_ne_bytes());
            bytes[4..6].copy_from_slice(&id.product.to_ne_bytes());
            bytes[6..8].copy_from_slice(&id.version.to_ne_bytes());
            copy_out(call.user, call.argument, &bytes).map(|()| 0)
        }
        // EVIOCGRAB/EVIOCREVOKE 按 Linux 语义把 argument 解释为标量。
        EVIOCGRAB => InputFile::set_grab(file, call.argument != 0)
            .map(|()| 0)
            .map_err(input_error),
        EVIOCREVOKE => {
            if call.argument != 0 {
                Err(DeviceError::Errno(errno::EINVAL))
            } else {
                InputFile::revoke(file).map(|()| 0).map_err(input_error)
            }
        }
        EVIOCSCLOCKID => copy_in_i32(call.user, call.argument)
            .and_then(|clock| file.set_clock(clock).map_err(input_error))
            .map(|()| 0),
        _ => copy_variable(file, call),
    }
}
