use crate::{
    drm::{DrmFile, DumbBufferInfo},
    syscall::errno,
    task::TaskControlBlock,
};

use super::{
    DRM_FORMAT_XRGB8888, copy_in, copy_out, drm_errno, read_u32, read_u64, write_u32, write_u64,
};

pub(super) fn create_dumb(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut bytes = copy_in::<32>(task, argument)?;
    let prepared = file
        .prepare_dumb(
            read_u32(&bytes, 4)?,
            read_u32(&bytes, 0)?,
            read_u32(&bytes, 8)?,
            read_u32(&bytes, 12)?,
        )
        .map_err(drm_errno)?;
    prepared.complete(|info| publish_dumb(task, argument, &mut bytes, info))
}

fn publish_dumb(
    task: &TaskControlBlock,
    argument: usize,
    bytes: &mut [u8; 32],
    info: DumbBufferInfo,
) -> Result<(), isize> {
    // Linux 保留 caller 的 height/width/bpp/flags，只覆盖三个 output 字段。
    write_u32(bytes, 16, info.handle)?;
    write_u32(bytes, 20, info.pitch)?;
    write_u64(bytes, 24, info.size)?;
    copy_out(task, argument, bytes)
}

pub(super) fn add_framebuffer(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut bytes = copy_in::<28>(task, argument)?;
    if read_u32(&bytes, 16)? != 32 || read_u32(&bytes, 20)? != 24 {
        return Err(errno::EINVAL);
    }
    let prepared = file
        .prepare_framebuffer(
            read_u32(&bytes, 24)?,
            read_u32(&bytes, 4)?,
            read_u32(&bytes, 8)?,
            read_u32(&bytes, 12)?,
        )
        .map_err(drm_errno)?;
    prepared.complete(|id| publish_framebuffer_id(task, argument, &mut bytes, id))
}

pub(super) fn add_framebuffer2(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut bytes = copy_in::<104>(task, argument)?;
    if read_u32(&bytes, 12)? != DRM_FORMAT_XRGB8888
        || read_u32(&bytes, 16)? != 0
        || (1..4).any(|plane| {
            read_u32(&bytes, 20 + plane * 4).unwrap_or(1) != 0
                || read_u32(&bytes, 36 + plane * 4).unwrap_or(1) != 0
        })
        || (0..4).any(|plane| {
            read_u32(&bytes, 52 + plane * 4).unwrap_or(1) != 0
                || read_u64(&bytes, 72 + plane * 8).unwrap_or(1) != 0
        })
    {
        return Err(errno::EINVAL);
    }
    let prepared = file
        .prepare_framebuffer(
            read_u32(&bytes, 20)?,
            read_u32(&bytes, 4)?,
            read_u32(&bytes, 8)?,
            read_u32(&bytes, 36)?,
        )
        .map_err(drm_errno)?;
    prepared.complete(|id| publish_framebuffer_id(task, argument, &mut bytes, id))
}

fn publish_framebuffer_id<const N: usize>(
    task: &TaskControlBlock,
    argument: usize,
    bytes: &mut [u8; N],
    id: u32,
) -> Result<(), isize> {
    write_u32(bytes, 0, id)?;
    copy_out(task, argument, bytes)
}
