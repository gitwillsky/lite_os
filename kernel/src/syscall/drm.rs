use alloc::sync::Arc;

use crate::{
    drm::{DisplayRect, DrmError, DrmFile, DrmWait, FramebufferRemoval},
    ipc::PipeWaitCondition,
    task::{TaskControlBlock, WaitResult, wait_for_pipe},
};

use super::errno;

mod publication;

const IOC_WRITE: usize = 1;
const IOC_READ: usize = 2;
const DRM_IOCTL_BASE: usize = b'd' as usize;
const CRTC_ID: u32 = 1;
const ENCODER_ID: u32 = 2;
const CONNECTOR_ID: u32 = 3;
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

const fn drm_ioc(direction: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | DRM_IOCTL_BASE << 8 | number
}

const DRM_IOCTL_VERSION: usize = drm_ioc(IOC_READ | IOC_WRITE, 0x00, 64);
const DRM_IOCTL_GET_CAP: usize = drm_ioc(IOC_READ | IOC_WRITE, 0x0c, 16);
const DRM_IOCTL_SET_MASTER: usize = drm_ioc(0, 0x1e, 0);
const DRM_IOCTL_DROP_MASTER: usize = drm_ioc(0, 0x1f, 0);
const DRM_IOCTL_MODE_GETRESOURCES: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xa0, 64);
const DRM_IOCTL_MODE_GETCRTC: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xa1, 104);
const DRM_IOCTL_MODE_SETCRTC: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xa2, 104);
const DRM_IOCTL_MODE_GETENCODER: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xa6, 20);
const DRM_IOCTL_MODE_GETCONNECTOR: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xa7, 80);
const DRM_IOCTL_MODE_GETFB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xad, 28);
const DRM_IOCTL_MODE_ADDFB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xae, 28);
const DRM_IOCTL_MODE_RMFB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xaf, 4);
const DRM_IOCTL_MODE_PAGE_FLIP: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xb0, 24);
const DRM_IOCTL_MODE_DIRTYFB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xb1, 24);
const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xb2, 32);
const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xb3, 16);
const DRM_IOCTL_MODE_DESTROY_DUMB: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xb4, 4);
const DRM_IOCTL_MODE_ADDFB2: usize = drm_ioc(IOC_READ | IOC_WRITE, 0xb8, 104);

/// @description 分发 Linux DRM/KMS topology query 与 dumb-buffer ioctl 子集。
///
/// @param task 当前 userspace address-space owner。
/// @param file `/dev/dri/card0` 打开的 DRM OFD backend。
/// @param request Linux DRM ioctl number，size/direction 必须精确匹配 RV64 UAPI。
/// @param argument request structure 的 userspace address。
/// @return 成功返回零；pointer、object ID 或未支持 request 返回负 errno。
pub(in crate::syscall) fn drm_ioctl(
    task: &TaskControlBlock,
    file: &Arc<DrmFile>,
    request: usize,
    argument: usize,
) -> isize {
    let result = match request {
        DRM_IOCTL_VERSION => version(task, argument),
        DRM_IOCTL_GET_CAP => get_cap(task, argument),
        DRM_IOCTL_SET_MASTER => set_master(task, file),
        DRM_IOCTL_DROP_MASTER => drop_master(task, file),
        DRM_IOCTL_MODE_GETRESOURCES => resources(task, file, argument),
        DRM_IOCTL_MODE_GETCRTC => crtc(task, file, argument),
        DRM_IOCTL_MODE_SETCRTC => set_crtc(task, file, argument),
        DRM_IOCTL_MODE_GETENCODER => encoder(task, argument),
        DRM_IOCTL_MODE_GETCONNECTOR => connector(task, file, argument),
        DRM_IOCTL_MODE_GETFB => framebuffer(task, file, argument),
        DRM_IOCTL_MODE_ADDFB => publication::add_framebuffer(task, file, argument),
        DRM_IOCTL_MODE_RMFB => remove_framebuffer(task, file, argument),
        DRM_IOCTL_MODE_PAGE_FLIP => page_flip(task, file, argument),
        DRM_IOCTL_MODE_DIRTYFB => dirty_framebuffer(task, file, argument),
        DRM_IOCTL_MODE_CREATE_DUMB => publication::create_dumb(task, file, argument),
        DRM_IOCTL_MODE_MAP_DUMB => map_dumb(task, file, argument),
        DRM_IOCTL_MODE_DESTROY_DUMB => destroy_dumb(task, file, argument),
        DRM_IOCTL_MODE_ADDFB2 => publication::add_framebuffer2(task, file, argument),
        _ => return -errno::ENOTTY,
    };
    result.map_or_else(|error| -error, |()| 0)
}

fn version(task: &TaskControlBlock, argument: usize) -> Result<(), isize> {
    const NAME: &[u8] = b"liteos";
    const DATE: &[u8] = b"20260714";
    const DESCRIPTION: &[u8] = b"LiteOS VirtIO GPU";
    let mut bytes = copy_in::<64>(task, argument)?;
    let name_length = read_u64(&bytes, 16)?;
    let name_pointer = read_u64(&bytes, 24)?;
    let date_length = read_u64(&bytes, 32)?;
    let date_pointer = read_u64(&bytes, 40)?;
    let description_length = read_u64(&bytes, 48)?;
    let description_pointer = read_u64(&bytes, 56)?;

    copy_string(task, name_pointer, name_length, NAME)?;
    copy_string(task, date_pointer, date_length, DATE)?;
    copy_string(task, description_pointer, description_length, DESCRIPTION)?;
    bytes.fill(0);
    write_u32(&mut bytes, 0, 1)?;
    write_u64(&mut bytes, 16, NAME.len() as u64)?;
    write_u64(&mut bytes, 24, name_pointer)?;
    write_u64(&mut bytes, 32, DATE.len() as u64)?;
    write_u64(&mut bytes, 40, date_pointer)?;
    write_u64(&mut bytes, 48, DESCRIPTION.len() as u64)?;
    write_u64(&mut bytes, 56, description_pointer)?;
    copy_out(task, argument, &bytes)
}

fn get_cap(task: &TaskControlBlock, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<16>(task, argument)?;
    let value = match read_u64(&bytes, 0)? {
        1 => 1,
        2 => 1,
        3 => 24,
        4 => 1,
        6 => 1,
        0x12 => 1,
        5 | 7 | 8 | 9 | 0x10 | 0x11 | 0x13 | 0x14 | 0x15 => 0,
        _ => return Err(errno::EINVAL),
    };
    write_u64(&mut bytes, 8, value)?;
    copy_out(task, argument, &bytes)
}

fn set_master(task: &TaskControlBlock, file: &DrmFile) -> Result<(), isize> {
    file.set_master(task.credential_id(true, true) == 0)
        .map_err(drm_errno)
}

fn drop_master(task: &TaskControlBlock, file: &DrmFile) -> Result<(), isize> {
    file.drop_master(task.credential_id(true, true) == 0)
        .map_err(drm_errno)
}

fn map_dumb(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<16>(task, argument)?;
    let offset = file.map_dumb(read_u32(&bytes, 0)?).map_err(drm_errno)?;
    write_u64(&mut bytes, 8, offset)?;
    copy_out(task, argument, &bytes)
}

fn destroy_dumb(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let bytes = copy_in::<4>(task, argument)?;
    file.destroy_dumb(read_u32(&bytes, 0)?)
        .map_err(|error| match error {
            // Linux drm_gem_handle_delete 对 file-private handle miss 返回 EINVAL。
            DrmError::NotFound => errno::EINVAL,
            error => drm_errno(error),
        })
}

fn framebuffer(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<28>(task, argument)?;
    let id = read_u32(&bytes, 0)?;
    let info = file.framebuffer(id).map_err(drm_errno)?;
    bytes.fill(0);
    write_u32(&mut bytes, 0, id)?;
    write_u32(&mut bytes, 4, info.width)?;
    write_u32(&mut bytes, 8, info.height)?;
    write_u32(&mut bytes, 12, info.pitch)?;
    write_u32(&mut bytes, 16, 32)?;
    write_u32(&mut bytes, 20, 24)?;
    write_u32(&mut bytes, 24, info.handle)?;
    copy_out(task, argument, &bytes)
}

fn remove_framebuffer(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<4>(task, argument)?;
    let id = read_u32(&bytes, 0)?;
    loop {
        match file.remove_framebuffer(id).map_err(drm_errno)? {
            FramebufferRemoval::Removed => return Ok(()),
            FramebufferRemoval::Wait(wait) => wait_scanout(wait)?,
        }
    }
}

pub(in crate::syscall) fn drm_errno(error: DrmError) -> isize {
    match error {
        DrmError::Invalid => errno::EINVAL,
        DrmError::NotFound => errno::ENOENT,
        DrmError::OutOfMemory => errno::ENOMEM,
        DrmError::NoSpace => errno::ENOSPC,
        DrmError::Busy => errno::EBUSY,
        DrmError::Device => errno::EIO,
        DrmError::Permission => errno::EACCES,
    }
}

fn resources(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<64>(task, argument)?;
    copy_framebuffer_ids(task, file, read_u64(&bytes, 0)?, read_u32(&bytes, 32)?)?;
    copy_id_array(task, read_u64(&bytes, 8)?, read_u32(&bytes, 36)?, CRTC_ID)?;
    copy_id_array(
        task,
        read_u64(&bytes, 16)?,
        read_u32(&bytes, 40)?,
        CONNECTOR_ID,
    )?;
    copy_id_array(
        task,
        read_u64(&bytes, 24)?,
        read_u32(&bytes, 44)?,
        ENCODER_ID,
    )?;
    let mode = file.mode();
    write_u32(
        &mut bytes,
        32,
        u32::try_from(file.framebuffer_count()).unwrap_or(u32::MAX),
    )?;
    write_u32(&mut bytes, 36, 1)?;
    write_u32(&mut bytes, 40, 1)?;
    write_u32(&mut bytes, 44, 1)?;
    write_u32(&mut bytes, 48, mode.hdisplay.into())?;
    write_u32(&mut bytes, 52, mode.hdisplay.into())?;
    write_u32(&mut bytes, 56, mode.vdisplay.into())?;
    write_u32(&mut bytes, 60, mode.vdisplay.into())?;
    copy_out(task, argument, &bytes)
}

fn copy_framebuffer_ids(
    task: &TaskControlBlock,
    file: &DrmFile,
    pointer: u64,
    capacity: u32,
) -> Result<(), isize> {
    for index in 0..capacity as usize {
        let Some(id) = file.framebuffer_id(index) else {
            break;
        };
        let address = usize::try_from(pointer)
            .ok()
            .and_then(|pointer| pointer.checked_add(index * 4))
            .ok_or(errno::EFAULT)?;
        copy_out(task, address, &id.to_ne_bytes())?;
    }
    Ok(())
}

fn crtc(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<104>(task, argument)?;
    if read_u32(&bytes, 12)? != CRTC_ID {
        return Err(errno::ENOENT);
    }
    bytes.fill(0);
    write_u32(&mut bytes, 12, CRTC_ID)?;
    if let Some((framebuffer, active_mode)) = file.active_crtc() {
        write_u32(&mut bytes, 16, framebuffer)?;
        write_u32(&mut bytes, 32, 1)?;
        let mut mode = [0u8; 68];
        encode_mode(&mut mode, active_mode)?;
        bytes[36..104].copy_from_slice(&mode);
    }
    copy_out(task, argument, &bytes)
}

fn set_crtc(task: &TaskControlBlock, file: &Arc<DrmFile>, argument: usize) -> Result<(), isize> {
    let bytes = copy_in::<104>(task, argument)?;
    if read_u32(&bytes, 12)? != CRTC_ID || read_u32(&bytes, 20)? != 0 || read_u32(&bytes, 24)? != 0
    {
        return Err(errno::EINVAL);
    }
    if read_u32(&bytes, 16)? == 0 && read_u32(&bytes, 8)? == 0 && read_u32(&bytes, 32)? == 0 {
        let wait = file.disable_crtc().map_err(drm_errno)?;
        return wait_scanout(wait);
    }
    let mode = file.mode();
    if read_u32(&bytes, 8)? != 1
        || read_u32(&bytes, 16)? == 0
        || read_u32(&bytes, 32)? != 1
        || read_u16(&bytes, 40)? != mode.hdisplay
        || read_u16(&bytes, 50)? != mode.vdisplay
    {
        return Err(errno::EINVAL);
    }
    let connector_pointer = read_u64(&bytes, 0)?;
    let connector_address = usize::try_from(connector_pointer).map_err(|_| errno::EFAULT)?;
    if read_u32(&copy_in::<4>(task, connector_address)?, 0)? != CONNECTOR_ID {
        return Err(errno::ENOENT);
    }
    let wait = file.set_crtc(read_u32(&bytes, 16)?).map_err(drm_errno)?;
    wait_scanout(wait)
}

fn page_flip(task: &TaskControlBlock, file: &Arc<DrmFile>, argument: usize) -> Result<(), isize> {
    let bytes = copy_in::<24>(task, argument)?;
    let flags = read_u32(&bytes, 8)?;
    if read_u32(&bytes, 0)? != CRTC_ID
        || flags & !1 != 0
        || read_u32(&bytes, 12)? != 0
        || file.active_framebuffer().is_none()
    {
        return Err(errno::EINVAL);
    }
    let user_data = (flags & 1 != 0).then(|| read_u64(&bytes, 16)).transpose()?;
    file.page_flip(read_u32(&bytes, 4)?, user_data)
        .map(|_| ())
        .map_err(drm_errno)
}

fn dirty_framebuffer(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    const MAX_CLIPS: usize = 32;
    const ANNOTATE_COPY: u32 = 1;

    let bytes = copy_in::<24>(task, argument)?;
    let framebuffer = read_u32(&bytes, 0)?;
    let flags = read_u32(&bytes, 4)? & 3;
    let count = usize::try_from(read_u32(&bytes, 12)?).map_err(|_| errno::EINVAL)?;
    let pointer = read_u64(&bytes, 16)?;
    if (count == 0) != (pointer == 0)
        || count > MAX_CLIPS
        || flags & ANNOTATE_COPY != 0 && !count.is_multiple_of(2)
    {
        return Err(errno::EINVAL);
    }

    let mut rectangles = [DisplayRect::default(); MAX_CLIPS];
    for (index, rectangle) in rectangles[..count].iter_mut().enumerate() {
        let address = usize::try_from(pointer)
            .ok()
            .and_then(|pointer| pointer.checked_add(index * 8))
            .ok_or(errno::EFAULT)?;
        let clip = copy_in::<8>(task, address)?;
        let x1 = u32::from(read_u16(&clip, 0)?);
        let y1 = u32::from(read_u16(&clip, 2)?);
        let x2 = u32::from(read_u16(&clip, 4)?);
        let y2 = u32::from(read_u16(&clip, 6)?);
        *rectangle = DisplayRect {
            x: x1,
            y: y1,
            width: x2
                .checked_sub(x1)
                .filter(|width| *width != 0)
                .ok_or(errno::EINVAL)?,
            height: y2
                .checked_sub(y1)
                .filter(|height| *height != 0)
                .ok_or(errno::EINVAL)?,
        };
    }
    let wait = file
        .dirty_framebuffer(framebuffer, &rectangles[..count])
        .map_err(drm_errno)?;
    wait_scanout(wait)
}

fn wait_scanout(wait: DrmWait) -> Result<(), isize> {
    loop {
        let Some(pipe) = wait.prepare_to_block() else {
            return Ok(());
        };
        match wait_for_pipe(&pipe, PipeWaitCondition::Readable) {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(errno::EINTR),
            WaitResult::OutOfMemory => return Err(errno::ENOMEM),
            WaitResult::TimedOut => panic!("DRM completion wait has no timeout"),
        }
    }
}

fn encoder(task: &TaskControlBlock, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<20>(task, argument)?;
    if read_u32(&bytes, 0)? != ENCODER_ID {
        return Err(errno::ENOENT);
    }
    bytes.fill(0);
    write_u32(&mut bytes, 0, ENCODER_ID)?;
    write_u32(&mut bytes, 4, 5)?;
    write_u32(&mut bytes, 8, CRTC_ID)?;
    write_u32(&mut bytes, 12, 1)?;
    copy_out(task, argument, &bytes)
}

fn connector(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<80>(task, argument)?;
    if read_u32(&bytes, 48)? != CONNECTOR_ID {
        return Err(errno::ENOENT);
    }
    let encoder_pointer = read_u64(&bytes, 0)?;
    let mode_pointer = read_u64(&bytes, 8)?;
    let mode_capacity = read_u32(&bytes, 32)?;
    let encoder_capacity = read_u32(&bytes, 40)?;
    if encoder_capacity >= 1 {
        copy_u32(task, encoder_pointer, ENCODER_ID)?;
    }
    if mode_capacity >= 1 {
        let mut mode = [0u8; 68];
        encode_mode(&mut mode, file.mode())?;
        copy_out(
            task,
            usize::try_from(mode_pointer).map_err(|_| errno::EFAULT)?,
            &mode,
        )?;
    }
    bytes.fill(0);
    write_u64(&mut bytes, 0, encoder_pointer)?;
    write_u64(&mut bytes, 8, mode_pointer)?;
    write_u32(&mut bytes, 32, 1)?;
    write_u32(&mut bytes, 36, 0)?;
    write_u32(&mut bytes, 40, 1)?;
    write_u32(&mut bytes, 44, ENCODER_ID)?;
    write_u32(&mut bytes, 48, CONNECTOR_ID)?;
    write_u32(&mut bytes, 52, 15)?;
    write_u32(&mut bytes, 56, 1)?;
    write_u32(&mut bytes, 60, 1)?;
    copy_out(task, argument, &bytes)
}

fn encode_mode(bytes: &mut [u8; 68], mode: crate::drm::DrmMode) -> Result<(), isize> {
    write_u32(bytes, 0, mode.clock)?;
    write_u16(bytes, 4, mode.hdisplay)?;
    write_u16(bytes, 6, mode.hsync_start)?;
    write_u16(bytes, 8, mode.hsync_end)?;
    write_u16(bytes, 10, mode.htotal)?;
    write_u16(bytes, 14, mode.vdisplay)?;
    write_u16(bytes, 16, mode.vsync_start)?;
    write_u16(bytes, 18, mode.vsync_end)?;
    write_u16(bytes, 20, mode.vtotal)?;
    write_u32(bytes, 24, mode.vrefresh)?;
    write_u32(bytes, 28, mode.flags)?;
    write_u32(bytes, 32, mode.mode_type)?;
    let mut cursor = 36;
    cursor += write_decimal(&mut bytes[cursor..68], u32::from(mode.hdisplay));
    bytes[cursor] = b'x';
    cursor += 1;
    let _ = write_decimal(&mut bytes[cursor..68], u32::from(mode.vdisplay));
    Ok(())
}

fn write_decimal(output: &mut [u8], value: u32) -> usize {
    let mut reversed = [0u8; 10];
    let mut value = value;
    let mut length = 0;
    loop {
        reversed[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..length {
        output[index] = reversed[length - index - 1];
    }
    length
}

fn copy_id_array(
    task: &TaskControlBlock,
    pointer: u64,
    capacity: u32,
    id: u32,
) -> Result<(), isize> {
    if capacity >= 1 {
        copy_u32(task, pointer, id)?;
    }
    Ok(())
}

fn copy_u32(task: &TaskControlBlock, pointer: u64, value: u32) -> Result<(), isize> {
    copy_out(
        task,
        usize::try_from(pointer).map_err(|_| errno::EFAULT)?,
        &value.to_ne_bytes(),
    )
}

fn copy_string(
    task: &TaskControlBlock,
    pointer: u64,
    capacity: u64,
    value: &[u8],
) -> Result<(), isize> {
    let count = usize::try_from(capacity)
        .unwrap_or(usize::MAX)
        .min(value.len());
    if count == 0 {
        return Ok(());
    }
    copy_out(
        task,
        usize::try_from(pointer).map_err(|_| errno::EFAULT)?,
        &value[..count],
    )
}

fn copy_in<const N: usize>(task: &TaskControlBlock, address: usize) -> Result<[u8; N], isize> {
    let mut bytes = [0u8; N];
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| errno::EFAULT)?;
    Ok(bytes)
}

fn copy_out(task: &TaskControlBlock, address: usize, bytes: &[u8]) -> Result<(), isize> {
    if address == 0 {
        return Err(errno::EFAULT);
    }
    task.copy_to_user(address, bytes).map_err(|_| errno::EFAULT)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, isize> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(errno::EFAULT)?
            .try_into()
            .map_err(|_| errno::EFAULT)?,
    ))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, isize> {
    let value = bytes.get(offset..offset + 2).ok_or(errno::EFAULT)?;
    Ok(u16::from_ne_bytes([value[0], value[1]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, isize> {
    Ok(u64::from_ne_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(errno::EFAULT)?
            .try_into()
            .map_err(|_| errno::EFAULT)?,
    ))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), isize> {
    bytes
        .get_mut(offset..offset + 2)
        .ok_or(errno::EFAULT)?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), isize> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or(errno::EFAULT)?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<(), isize> {
    bytes
        .get_mut(offset..offset + 8)
        .ok_or(errno::EFAULT)?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}
