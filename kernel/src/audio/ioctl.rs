//! Linux 7.1 native 64-bit ALSA PCM ioctl codec。

use syscall_abi::errno;

use super::codec::{
    HW_PARAMS_BYTES, PCM_PROTOCOL_VERSION, PcmInfo, SNDRV_PCM_IOCTL_DELAY, SNDRV_PCM_IOCTL_DROP,
    SNDRV_PCM_IOCTL_HW_FREE, SNDRV_PCM_IOCTL_HW_PARAMS, SNDRV_PCM_IOCTL_HW_REFINE,
    SNDRV_PCM_IOCTL_INFO, SNDRV_PCM_IOCTL_PREPARE, SNDRV_PCM_IOCTL_PVERSION, SNDRV_PCM_IOCTL_START,
    SNDRV_PCM_IOCTL_STATUS, SNDRV_PCM_IOCTL_STATUS_EXT, SNDRV_PCM_IOCTL_SW_PARAMS,
    SNDRV_PCM_IOCTL_SYNC_PTR, SNDRV_PCM_IOCTL_TTSTAMP, SNDRV_PCM_IOCTL_USER_PVERSION,
    SNDRV_PCM_IOCTL_WRITEI_FRAMES, STATUS_BYTES, SW_PARAMS_BYTES, SYNC_PTR_BYTES, XFER_BYTES,
    choose_hardware, encode_pcm_info, exact_interval, exact_mask, read_u32, read_u64,
    refine_hardware, stop_or_error, write_u32, write_u64,
};
use super::{AudioError, HardwareParameters, PcmFile, SoftwareParameters};
use crate::fs::chardev::{self, DeviceError, IoctlCall, UserMemory};

const POLLOUT: i16 = 0x004;

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
        timestamp_mode: read_u32(bytes, 0)?,
        timestamp_type: (read_u32(bytes, 72)? >= 0x0002_000c)
            .then(|| read_u32(bytes, 76))
            .flatten(),
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

/// 写 `snd_pcm_hw_params` 里描述硬件能力的信息字段：`info`（`SNDRV_PCM_INFO_INTERLEAVED` 位）、
/// `msbits`、以及有理数形式的采样率 `rate_num / rate_den`。`HW_REFINE` 与 `HW_PARAMS` 都要返回它们。
fn describe_hardware(bytes: &mut [u8; HW_PARAMS_BYTES]) -> Result<(), isize> {
    let format = exact_mask(bytes, 36).and_then(super::codec::pcm_format);
    let rate = exact_interval(bytes, 11).unwrap_or(0);
    write_u32(bytes, 520, 0x0000_0100).ok_or(errno::EIO)?;
    write_u32(bytes, 524, format.map_or(0, |format| format.bits())).ok_or(errno::EIO)?;
    write_u32(bytes, 528, rate).ok_or(errno::EIO)?;
    write_u32(bytes, 532, 1).ok_or(errno::EIO)
}

fn copy_status(
    user: &dyn UserMemory,
    file: &PcmFile,
    argument: usize,
    extended: bool,
) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    if extended {
        // Linux STATUS_EXT 只读入 audio_tstamp_data；本设备仅提供参考时钟，不声明硬件音频时间戳，
        // 返回的 report/accuracy 维持零值，不能把请求中的类型和精度标志当成已实现能力原样返回。
        let mut configuration = [0u8; 4];
        user.read(
            argument.checked_add(92).ok_or(errno::EFAULT)?,
            &mut configuration,
        )
        .map_err(|_| errno::EFAULT)?;
    }
    let status = file.status();
    let mut bytes = [0u8; STATUS_BYTES];
    write_u32(&mut bytes, 0, status.state as i32 as u32).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 24, status.timestamp_ns / 1_000_000_000).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 32, status.timestamp_ns % 1_000_000_000).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 40, status.application_frames).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 48, status.hardware_frames).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 56, status.delay_frames as u64).ok_or(errno::EIO)?;
    let available = status.buffer_frames - status.delay_frames.max(0) as u64;
    write_u64(&mut bytes, 64, available).ok_or(errno::EIO)?;
    user.write(argument, &bytes).map_err(|_| errno::EFAULT)
}

fn write_frames(
    user: &dyn UserMemory,
    nonblocking: bool,
    file: &PcmFile,
    argument: usize,
) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let mut transfer = [0u8; XFER_BYTES];
    user.read(argument, &mut transfer)
        .map_err(|_| errno::EFAULT)?;
    let pointer = read_u64(&transfer, 8).ok_or(errno::EFAULT)? as usize;
    let frames = usize::try_from(read_u64(&transfer, 16).ok_or(errno::EFAULT)?)
        .map_err(|_| errno::EINVAL)?;
    let configuration = file.transfer_configuration();
    let frame_bytes = configuration.frame_bytes();
    let mut period = [0u8; crate::drivers::MAX_PCM_PERIOD_BYTES];
    let mut completed = 0usize;
    'transfer: while completed < frames {
        let count = (frames - completed).min(configuration.period_frames);
        let offset = completed
            .checked_mul(frame_bytes)
            .and_then(|offset| pointer.checked_add(offset))
            .ok_or(errno::EFAULT)?;
        if user
            .read(offset, &mut period[..count * frame_bytes])
            .is_err()
        {
            stop_or_error(completed, errno::EFAULT)?;
            break;
        }
        loop {
            match file.write_frames(&period[..count * frame_bytes], configuration) {
                Ok(written) => {
                    completed += written;
                    continue 'transfer;
                }
                Err(errno::EAGAIN) => {
                    if let Err(error) = chardev::wait_ready(file, POLLOUT, nonblocking) {
                        stop_or_error(completed, error.errno())?;
                        break 'transfer;
                    }
                }
                Err(error) => {
                    stop_or_error(completed, error)?;
                    break 'transfer;
                }
            }
        }
    }
    write_u64(&mut transfer, 0, completed as u64).ok_or(errno::EIO)?;
    user.write(argument, &transfer).map_err(|_| errno::EFAULT)
}

fn sync_pointer(user: &dyn UserMemory, file: &PcmFile, argument: usize) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let mut bytes = [0u8; SYNC_PTR_BYTES];
    user.read(argument, &mut bytes).map_err(|_| errno::EFAULT)?;
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
    write_u64(&mut bytes, 24, status.timestamp_ns / 1_000_000_000).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 32, status.timestamp_ns % 1_000_000_000).ok_or(errno::EIO)?;
    write_u64(&mut bytes, 72, status.application_frames).ok_or(errno::EIO)?;
    user.write(argument, &bytes).map_err(|_| errno::EFAULT)
}

/// 分发标准 ALSA consumer 使用的 Linux PCM ioctl 子集。
///
/// # Errors
///
/// 用户地址、参数、PCM 状态或 request 错误返回对应 errno。
pub(super) fn ioctl(file: &PcmFile, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    let (user, argument) = (call.user, call.argument);
    let result = match call.request {
        SNDRV_PCM_IOCTL_PVERSION => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                user.write(argument, &PCM_PROTOCOL_VERSION.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)
            }
        }
        SNDRV_PCM_IOCTL_USER_PVERSION => {
            // Linux 仅用此值决定 status/control mmap 的兼容布局。本实现不开放这两类 mmap，
            // 所有版本都走 native 64-bit SYNC_PTR，因此不存在需保留的版本相关状态。
            let mut version = [0u8; 4];
            user.read(argument, &mut version).map_err(|_| errno::EFAULT)
        }
        SNDRV_PCM_IOCTL_TTSTAMP => {
            let mut kind = [0u8; 4];
            user.read(argument, &mut kind)
                .map_err(|_| errno::EFAULT)
                .and_then(|()| file.timestamp_type(u32::from_ne_bytes(kind)))
        }
        SNDRV_PCM_IOCTL_INFO => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                let (subdevice, count, available) = file.subdevices();
                let info = encode_pcm_info(&PcmInfo {
                    device: file.endpoint.device(),
                    card: file.card.index,
                    subdevice: subdevice as u32,
                    subdevices_count: count as u32,
                    subdevices_available: available as u32,
                    card_id: file.card.id.as_bytes(),
                    name: file.endpoint.name(),
                });
                user.write(argument, &info).map_err(|_| errno::EFAULT)
            }
        }
        SNDRV_PCM_IOCTL_HW_REFINE => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                let mut bytes = [0u8; HW_PARAMS_BYTES];
                user.read(argument, &mut bytes)
                    .map_err(|_| errno::EFAULT)
                    .and_then(|()| {
                        refine_hardware(&mut bytes, file.card.output.capabilities(), file.endpoint)
                            .map_err(|()| errno::EINVAL)
                    })
                    .and_then(|()| {
                        describe_hardware(&mut bytes)?;
                        user.write(argument, &bytes).map_err(|_| errno::EFAULT)
                    })
            }
        }
        SNDRV_PCM_IOCTL_HW_PARAMS => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                let mut bytes = [0u8; HW_PARAMS_BYTES];
                user.read(argument, &mut bytes)
                    .map_err(|_| errno::EFAULT)
                    .and_then(|()| {
                        choose_hardware(&mut bytes, file.card.output.capabilities(), file.endpoint)
                            .map_err(|()| errno::EINVAL)
                    })
                    .and_then(|()| decode_hardware(&bytes).ok_or(errno::EINVAL))
                    .and_then(|parameters| file.hardware_parameters(parameters).map_err(pcm_errno))
                    .and_then(|()| {
                        describe_hardware(&mut bytes)?;
                        user.write(argument, &bytes).map_err(|_| errno::EFAULT)
                    })
            }
        }
        SNDRV_PCM_IOCTL_HW_FREE => file.free_hardware().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_SW_PARAMS => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                let mut bytes = [0u8; SW_PARAMS_BYTES];
                user.read(argument, &mut bytes)
                    .map_err(|_| errno::EFAULT)
                    .and_then(|()| decode_software(&bytes).ok_or(errno::EINVAL))
                    .and_then(|parameters| file.software_parameters(parameters))
                    .and_then(|()| user.write(argument, &bytes).map_err(|_| errno::EFAULT))
            }
        }
        SNDRV_PCM_IOCTL_STATUS | SNDRV_PCM_IOCTL_STATUS_EXT => copy_status(
            user,
            file,
            argument,
            call.request == SNDRV_PCM_IOCTL_STATUS_EXT,
        ),
        SNDRV_PCM_IOCTL_DELAY => {
            if argument == 0 {
                Err(errno::EFAULT)
            } else {
                user.write(argument, &file.status().delay_frames.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)
            }
        }
        SNDRV_PCM_IOCTL_SYNC_PTR => sync_pointer(user, file, argument),
        SNDRV_PCM_IOCTL_PREPARE => file.prepare().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_START => file.start().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_DROP => file.drop_stream().map_err(pcm_errno),
        SNDRV_PCM_IOCTL_WRITEI_FRAMES => write_frames(user, call.nonblocking, file, argument),
        _ => Err(errno::ENOTTY),
    };
    result.map(|()| 0).map_err(DeviceError::Errno)
}
