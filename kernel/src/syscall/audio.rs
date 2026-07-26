//! @description Linux 7.1 native 64-bit ALSA PCM ioctl codec。

use alloc::sync::Arc;

use crate::{
    audio::{
        AudioError, HardwareParameters, PCM_BUFFER_FRAMES, PCM_FRAME_BYTES, PCM_PERIOD_BYTES,
        PCM_PERIOD_FRAMES, PcmFile, SoftwareParameters,
    },
    fs::{O_NONBLOCK, OpenFileDescription},
    task::{TaskControlBlock, WaitResult},
};

use super::errno;

#[path = "audio/codec.rs"]
mod codec;
use codec::{
    HW_PARAMS_BYTES, PCM_PROTOCOL_VERSION, STATUS_BYTES, SW_PARAMS_BYTES, SYNC_PTR_BYTES,
    XFER_BYTES, exact_interval, exact_mask, read_u32, read_u64, stop_or_error, write_u32,
    write_u64,
};
pub(in crate::syscall) use codec::{
    SNDRV_PCM_IOCTL_DELAY, SNDRV_PCM_IOCTL_DROP, SNDRV_PCM_IOCTL_HW_FREE,
    SNDRV_PCM_IOCTL_HW_PARAMS, SNDRV_PCM_IOCTL_PREPARE, SNDRV_PCM_IOCTL_PVERSION,
    SNDRV_PCM_IOCTL_START, SNDRV_PCM_IOCTL_STATUS, SNDRV_PCM_IOCTL_SW_PARAMS,
    SNDRV_PCM_IOCTL_SYNC_PTR, SNDRV_PCM_IOCTL_WRITEI_FRAMES,
};

fn decode_hardware(bytes: &[u8; HW_PARAMS_BYTES]) -> Option<HardwareParameters> {
    Some(HardwareParameters {
        access: exact_mask(bytes, 4)?,
        format: exact_mask(bytes, 36)?,
        channels: exact_interval(bytes, 10)?,
        rate: exact_interval(bytes, 11)?,
        period_frames: u64::from(exact_interval(bytes, 13)?),
        periods: exact_interval(bytes, 15)?,
        buffer_frames: u64::from(exact_interval(bytes, 17)?),
    })
}

fn decode_software(bytes: &[u8; SW_PARAMS_BYTES]) -> Option<SoftwareParameters> {
    Some(SoftwareParameters {
        available_min: read_u64(bytes, 16)?,
        start_threshold: read_u64(bytes, 32)?,
        stop_threshold: read_u64(bytes, 40)?,
        boundary: read_u64(bytes, 64)?,
    })
}

fn pcm_errno(error: AudioError) -> isize {
    match error {
        AudioError::WouldBlock => errno::EAGAIN,
        AudioError::InvalidState => errno::EBADFD,
        AudioError::Device => errno::EIO,
    }
}

fn copy_status(task: &TaskControlBlock, file: &PcmFile, argument: usize) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let status = file.status();
    let mut bytes = [0u8; STATUS_BYTES];
    write_u32(&mut bytes, 0, status.state as i32 as u32).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 40, status.application_frames).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 48, status.hardware_frames).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 56, status.delay_frames as u64).ok_or(errno::EIO)?;
    let available = PCM_BUFFER_FRAMES as u64 - status.delay_frames.max(0) as u64;
    write_u64(&mut bytes, 64, available).ok_or(errno::EIO)?;
    task.copy_to_user(argument, &bytes)
        .map_err(|_| errno::EFAULT)
}

fn write_frames(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    file: &PcmFile,
    argument: usize,
) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let mut transfer = [0u8; XFER_BYTES];
    task.copy_from_user(argument, &mut transfer)
        .map_err(|_| errno::EFAULT)?;
    let pointer = read_u64(&transfer, 8).ok_or(errno::EFAULT)? as usize;
    let frames = usize::try_from(read_u64(&transfer, 16).ok_or(errno::EFAULT)?)
        .map_err(|_| errno::EINVAL)?;
    if frames == 0 || !frames.is_multiple_of(PCM_PERIOD_FRAMES) {
        return Err(errno::EINVAL);
    }
    let mut period = [0u8; PCM_PERIOD_BYTES];
    let mut completed = 0usize;
    'transfer: while completed < frames {
        let offset = completed
            .checked_mul(PCM_FRAME_BYTES)
            .and_then(|offset| pointer.checked_add(offset))
            .ok_or(errno::EFAULT)?;
        if task.copy_from_user(offset, &mut period).is_err() {
            stop_or_error(completed, errno::EFAULT)?;
            break;
        }
        loop {
            match file.write_period(&period) {
                Ok(()) => {
                    completed += PCM_PERIOD_FRAMES;
                    continue 'transfer;
                }
                Err(AudioError::WouldBlock) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                    stop_or_error(completed, errno::EAGAIN)?;
                    break 'transfer;
                }
                Err(AudioError::WouldBlock) => {
                    match crate::syscall::poll::wait_for_ofd(ofd, 0x004) {
                        WaitResult::Woken => {}
                        WaitResult::Interrupted => {
                            stop_or_error(completed, errno::EINTR)?;
                            break 'transfer;
                        }
                        WaitResult::TimedOut => unreachable!(),
                        WaitResult::OutOfMemory => {
                            stop_or_error(completed, errno::ENOMEM)?;
                            break 'transfer;
                        }
                    }
                }
                Err(error) => {
                    stop_or_error(completed, pcm_errno(error))?;
                    break 'transfer;
                }
            }
        }
    }
    write_u64(&mut transfer, 0, completed as u64).ok_or(errno::EIO)?;
    task.copy_to_user(argument, &transfer)
        .map_err(|_| errno::EFAULT)
}

fn sync_pointer(task: &TaskControlBlock, file: &PcmFile, argument: usize) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let mut bytes = [0u8; SYNC_PTR_BYTES];
    task.copy_from_user(argument, &mut bytes)
        .map_err(|_| errno::EFAULT)?;
    let flags = read_u32(&bytes, 0).ok_or(errno::EFAULT)?;
    if flags & !0x7 != 0 {
        return Err(errno::EINVAL);
    }
    if flags & 0x2 == 0 {
        file.commit_application_pointer(read_u64(&bytes, 72).ok_or(errno::EFAULT)?)
            .map_err(pcm_errno)?;
    }
    let status = file.status();
    write_u32(&mut bytes, 8, status.state as i32 as u32).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 16, status.hardware_frames).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 72, status.application_frames).ok_or(errno::EIO)?;
    task.copy_to_user(argument, &bytes)
        .map_err(|_| errno::EFAULT)
}

/// @description 分发系统音频服务消费的 Linux ALSA PCM ioctl 子集。
pub(in crate::syscall) fn audio_ioctl(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    file: &Arc<PcmFile>,
    request: usize,
    argument: usize,
) -> isize {
    let result = match request {
        SNDRV_PCM_IOCTL_PVERSION => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                task.copy_to_user(argument, &PCM_PROTOCOL_VERSION.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)
            }
        }
        SNDRV_PCM_IOCTL_HW_PARAMS => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                let mut bytes = [0u8; HW_PARAMS_BYTES];
                task.copy_from_user(argument, &mut bytes)
                    .map_err(|_| errno::EFAULT)
                    .and_then(|()| decode_hardware(&bytes).ok_or(errno::EINVAL))
                    .and_then(|parameters| file.hardware_parameters(parameters).map_err(pcm_errno))
                    .and_then(|()| {
                        write_u32(&mut bytes, 520, 0x0000_0100).ok_or(errno::EIO)?;
                        write_u32(&mut bytes, 524, 32).ok_or(errno::EIO)?;
                        write_u32(&mut bytes, 528, 48_000).ok_or(errno::EIO)?;
                        write_u32(&mut bytes, 532, 1).ok_or(errno::EIO)?;
                        task.copy_to_user(argument, &bytes)
                            .map_err(|_| errno::EFAULT)
                    })
            }
        }
        SNDRV_PCM_IOCTL_HW_FREE => file.free_hardware().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_SW_PARAMS => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                let mut bytes = [0u8; SW_PARAMS_BYTES];
                task.copy_from_user(argument, &mut bytes)
                    .map_err(|_| errno::EFAULT)
                    .and_then(|()| decode_software(&bytes).ok_or(errno::EINVAL))
                    .and_then(|parameters| file.software_parameters(parameters).map_err(pcm_errno))
                    .and_then(|()| {
                        task.copy_to_user(argument, &bytes)
                            .map_err(|_| errno::EFAULT)
                    })
            }
        }
        SNDRV_PCM_IOCTL_STATUS => copy_status(task, file, argument),
        SNDRV_PCM_IOCTL_DELAY => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                task.copy_to_user(argument, &file.status().delay_frames.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)
            }
        }
        SNDRV_PCM_IOCTL_SYNC_PTR => sync_pointer(task, file, argument),
        SNDRV_PCM_IOCTL_PREPARE => file.prepare().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_START => file.start().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_DROP => file.drop_stream().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_WRITEI_FRAMES => write_frames(task, ofd, file, argument),
        _ => Err(errno::ENOTTY),
    };
    result.map_or_else(|error| -error, |()| 0)
}
