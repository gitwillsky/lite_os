use alloc::sync::Arc;

use crate::{
    input::{InputError, InputFile, InputString},
    task::TaskControlBlock,
};

use super::errno;

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

fn input_errno(error: InputError) -> isize {
    match error {
        InputError::NotFound => errno::ENOENT,
        InputError::OutOfMemory => errno::ENOMEM,
        InputError::Busy => errno::EBUSY,
        InputError::Invalid => errno::EINVAL,
        InputError::Revoked => errno::ENODEV,
    }
}

fn copy_out(task: &TaskControlBlock, argument: usize, bytes: &[u8]) -> Result<(), isize> {
    if bytes.is_empty() {
        return Ok(());
    }
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    task.copy_to_user(argument, bytes)
        .map_err(|_| errno::EFAULT)
}

fn copy_in_i32(task: &TaskControlBlock, argument: usize) -> Result<i32, isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let mut bytes = [0u8; 4];
    task.copy_from_user(argument, &mut bytes)
        .map_err(|_| errno::EFAULT)?;
    Ok(i32::from_ne_bytes(bytes))
}

fn copy_variable(
    task: &TaskControlBlock,
    file: &InputFile,
    request: usize,
    argument: usize,
) -> Result<isize, isize> {
    let direction = request >> 30 & 0x3;
    let size = request >> 16 & 0x3fff;
    let kind = request >> 8 & 0xff;
    let number = request & 0xff;
    if direction != IOC_READ || kind != INPUT_IOCTL_TYPE {
        return Err(errno::ENOTTY);
    }

    let mut bytes = [0u8; 129];
    let output_length = size.min(bytes.len());
    let count = match number {
        0x06 => file.copy_string(InputString::Name, &mut bytes[..output_length]),
        0x07 => file.copy_string(InputString::PhysicalPath, &mut bytes[..output_length]),
        0x08 => file.copy_string(InputString::Serial, &mut bytes[..output_length]),
        0x09 => file
            .copy_bitmap(None, &mut bytes[..output_length])
            .map_err(input_errno)?,
        0x18 => {
            let output = &mut bytes[..output_length];
            if !output.is_empty()
                && (argument == 0
                    || task
                        .validate_user_write(argument, output.len().min(96))
                        .is_err())
            {
                return Err(errno::EFAULT);
            }
            file.copy_key_state(output)
        }
        0x20..=0x3f => file
            .copy_bitmap(Some((number & 0x1f) as u16), &mut bytes[..output_length])
            .map_err(input_errno)?,
        0x40..=0x7f => {
            let info = file
                .absolute_info((number & 0x3f) as u16)
                .map_err(input_errno)?;
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
            copy_out(task, argument, &bytes[..count])?;
            return Ok(0);
        }
        _ => return Err(errno::ENOTTY),
    };
    if let Err(error) = copy_out(task, argument, &bytes[..count]) {
        if number == 0x18 {
            file.mark_sync_lost();
        }
        return Err(error);
    }
    Ok(count as isize)
}

/// @description 分发 Linux evdev query、clock 与 exclusive-grab ioctl 子集。
/// @param task 当前 userspace address-space owner。
/// @param file `/dev/input/eventN` 的独立 client backend。
/// @param request Linux input ioctl number。
/// @param argument request-specific pointer；`EVIOCGRAB/EVIOCREVOKE` 按 Linux 语义解释为标量。
/// @return fixed ioctl 返回零；variable query 返回复制 byte count；失败返回负 errno。
pub(in crate::syscall) fn input_ioctl(
    task: &TaskControlBlock,
    file: &Arc<InputFile>,
    request: usize,
    argument: usize,
) -> isize {
    if file.is_revoked() {
        return -errno::ENODEV;
    }
    let result = match request {
        EVIOCGVERSION => copy_out(task, argument, &EV_VERSION.to_ne_bytes()).map(|()| 0),
        EVIOCGID => {
            let id = file.id();
            let mut bytes = [0u8; 8];
            bytes[0..2].copy_from_slice(&id.bustype.to_ne_bytes());
            bytes[2..4].copy_from_slice(&id.vendor.to_ne_bytes());
            bytes[4..6].copy_from_slice(&id.product.to_ne_bytes());
            bytes[6..8].copy_from_slice(&id.version.to_ne_bytes());
            copy_out(task, argument, &bytes).map(|()| 0)
        }
        EVIOCGRAB => InputFile::set_grab(file, argument != 0)
            .map(|()| 0)
            .map_err(input_errno),
        EVIOCREVOKE => {
            if argument != 0 {
                Err(errno::EINVAL)
            } else {
                InputFile::revoke(file).map(|()| 0).map_err(input_errno)
            }
        }
        EVIOCSCLOCKID => copy_in_i32(task, argument)
            .and_then(|clock| file.set_clock(clock).map_err(input_errno))
            .map(|()| 0),
        _ => copy_variable(task, file, request, argument),
    };
    result.unwrap_or_else(|error| -error)
}
