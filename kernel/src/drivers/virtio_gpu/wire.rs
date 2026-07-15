use super::DisplayMode;

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
pub(super) const CONTROL_HEADER_SIZE: usize = 24;
pub(super) const DISPLAY_INFO_SIZE: usize = CONTROL_HEADER_SIZE + 16 * 24;
pub(super) const BOOT_RESOURCE_ID: u32 = 1;
pub(super) const ALTERNATE_RESOURCE_ID: u32 = 2;

pub(super) fn write_rect(bytes: &mut [u8], offset: usize, mode: DisplayMode) -> Option<()> {
    write_u32(bytes, offset, 0)?;
    write_u32(bytes, offset + 4, 0)?;
    write_u32(bytes, offset + 8, mode.width)?;
    write_u32(bytes, offset + 12, mode.height)
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
