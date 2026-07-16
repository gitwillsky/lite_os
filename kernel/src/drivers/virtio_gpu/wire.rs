use crate::memory::{DeviceBacking, PAGE_SIZE};

use super::{DisplayError, DisplayMode, DisplayRect};

pub(super) const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
pub(super) const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub(super) const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub(super) const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub(super) const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub(super) const VIRTIO_GPU_FLAG_FENCE: u32 = 1;
pub(super) const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub(super) const VIRTIO_GPU_EVENT_DISPLAY: u32 = 1;
pub(super) const VIRTIO_GPU_EVENTS_READ: usize = 0;
pub(super) const VIRTIO_GPU_EVENTS_CLEAR: usize = 4;
pub(super) const CONTROL_QUEUE: u32 = 0;
pub(super) const QUEUE_SIZE: u16 = 64;
pub(super) const ATTACH_REQUEST_SIZE: usize = 32 + DeviceBacking::MAX_EXTENTS * 16;
pub(super) const CONTROL_HEADER_SIZE: usize = 24;
pub(super) const DISPLAY_INFO_SIZE: usize = CONTROL_HEADER_SIZE + 16 * 24;
pub(super) const BOOT_RESOURCE_ID: u32 = 1;
pub(super) const ALTERNATE_RESOURCE_ID: u32 = 2;

pub(super) fn prepare_create(
    request: &mut [u8],
    mode: DisplayMode,
    resource_id: u32,
) -> Result<(), DisplayError> {
    request.fill(0);
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)?;
    write_u32(request, 28, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM).ok_or(DisplayError::Device)?;
    write_u32(request, 32, mode.width).ok_or(DisplayError::Device)?;
    write_u32(request, 36, mode.height).ok_or(DisplayError::Device)
}

pub(super) fn prepare_attach(
    request: &mut [u8],
    resource_id: u32,
    backing: &DeviceBacking,
) -> Result<usize, DisplayError> {
    request.fill(0);
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)?;
    write_u32(
        request,
        28,
        u32::try_from(backing.extent_count()).map_err(|_| DisplayError::Device)?,
    )
    .ok_or(DisplayError::Device)?;
    for index in 0..backing.extent_count() {
        let (ppn, pages) = backing.extent(index).ok_or(DisplayError::Device)?;
        let offset = 32 + index * 16;
        write_u64(request, offset, (ppn.as_usize() * PAGE_SIZE) as u64)
            .ok_or(DisplayError::Device)?;
        write_u32(
            request,
            offset + 8,
            u32::try_from(pages.checked_mul(PAGE_SIZE).ok_or(DisplayError::Device)?)
                .map_err(|_| DisplayError::InvalidRectangle)?,
        )
        .ok_or(DisplayError::Device)?;
    }
    Ok(32 + backing.extent_count() * 16)
}

pub(super) fn prepare_transfer(
    request: &mut [u8],
    mode: DisplayMode,
    rectangle: DisplayRect,
    resource_id: u32,
) -> Result<(), DisplayError> {
    request.fill(0);
    write_display_rect(request, 24, rectangle).ok_or(DisplayError::InvalidRectangle)?;
    let offset = u64::from(rectangle.y)
        .checked_mul(u64::from(mode.pitch))
        .and_then(|offset| offset.checked_add(u64::from(rectangle.x) * 4))
        .ok_or(DisplayError::InvalidRectangle)?;
    write_u64(request, 40, offset).ok_or(DisplayError::Device)?;
    write_u32(request, 48, resource_id).ok_or(DisplayError::Device)
}

pub(super) fn prepare_set_scanout(
    request: &mut [u8],
    mode: DisplayMode,
    resource_id: u32,
) -> Result<(), DisplayError> {
    request.fill(0);
    write_rect(request, 24, mode).ok_or(DisplayError::InvalidRectangle)?;
    write_u32(request, 40, 0).ok_or(DisplayError::Device)?;
    write_u32(request, 44, resource_id).ok_or(DisplayError::Device)
}

pub(super) fn prepare_flush(
    request: &mut [u8],
    rectangle: DisplayRect,
    resource_id: u32,
) -> Result<(), DisplayError> {
    request.fill(0);
    write_display_rect(request, 24, rectangle).ok_or(DisplayError::InvalidRectangle)?;
    write_u32(request, 40, resource_id).ok_or(DisplayError::Device)
}

pub(super) fn prepare_unref(request: &mut [u8], resource_id: u32) -> Result<(), DisplayError> {
    request.fill(0);
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)
}

pub(super) fn write_rect(bytes: &mut [u8], offset: usize, mode: DisplayMode) -> Option<()> {
    write_u32(bytes, offset, 0)?;
    write_u32(bytes, offset + 4, 0)?;
    write_u32(bytes, offset + 8, mode.width)?;
    write_u32(bytes, offset + 12, mode.height)
}

pub(super) fn write_display_rect(
    bytes: &mut [u8],
    offset: usize,
    rectangle: DisplayRect,
) -> Option<()> {
    write_u32(bytes, offset, rectangle.x)?;
    write_u32(bytes, offset + 4, rectangle.y)?;
    write_u32(bytes, offset + 8, rectangle.width)?;
    write_u32(bytes, offset + 12, rectangle.height)
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

pub(super) fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Option<()> {
    bytes
        .get_mut(offset..offset.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

pub(super) fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Option<()> {
    bytes
        .get_mut(offset..offset.checked_add(8)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}
