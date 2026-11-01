use native_runtime::native_abi::PAGE_SIZE;
use device_model::DeviceBacking;
use native_runtime::Bti;

use super::{DisplayError, DisplayMode, DisplayRect};

pub(super) const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
pub(super) const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub(super) const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub(super) const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub(super) const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x0109;
pub(super) const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
pub(super) const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x0201;
pub(super) const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub(super) const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
pub(super) const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
pub(super) const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
pub(super) const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;
pub(super) const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
pub(super) const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;
pub(super) const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub(super) const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub(super) const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub(super) const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
pub(super) const VIRTIO_GPU_FLAG_FENCE: u32 = 1;
pub(super) const VIRTIO_GPU_F_VIRGL: u64 = 1;
pub(super) const VIRTIO_GPU_F_CONTEXT_INIT: u64 = 1 << 4;
pub(super) const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub(super) const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub(super) const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub(super) const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub(super) const VIRTIO_GPU_EVENT_DISPLAY: u32 = 1;
pub(super) const VIRTIO_GPU_EVENTS_READ: usize = 0;
pub(super) const VIRTIO_GPU_EVENTS_CLEAR: usize = 4;
pub(super) const VIRTIO_GPU_NUM_CAPSETS: usize = 12;
pub(super) const CONTROL_QUEUE: u32 = 0;
pub(super) const CURSOR_QUEUE: u32 = 1;
pub(super) const QUEUE_SIZE: u16 = 64;
/// 最大的定长请求是 CTX_CREATE（含 64 字节调试名）。SUBMIT_3D 的命令流与 ATTACH_BACKING 的 entry
/// 数组不进槽位，作为独立 descriptor 从各自的 backing 读取。
pub(super) const CONTROL_REQUEST_SIZE: usize = 96;
pub(super) const CONTROL_HEADER_SIZE: usize = 24;
pub(super) const CURSOR_REQUEST_SIZE: usize = 56;
pub(super) const DISPLAY_INFO_SIZE: usize = CONTROL_HEADER_SIZE + 16 * 24;
pub(super) const CAPSET_INFO_SIZE: usize = CONTROL_HEADER_SIZE + 16;
pub(super) const MAX_CAPSET_SIZE: usize = 4096;
pub(super) const CAPSET_RESPONSE_SIZE: usize = CONTROL_HEADER_SIZE + MAX_CAPSET_SIZE;
// adapter 拥有同一 resource namespace 的保留前缀；新增角色放在 DynamicStart 前，
// 使 DRM 起点随之移动，不能与 boot 等仍存活的 host resource 撞号。
#[repr(u32)]
enum AdapterResource {
    Boot = 1,
    Alternate,
    Cursor,
    DynamicStart,
}
pub(super) const BOOT_RESOURCE_ID: u32 = AdapterResource::Boot as u32;
pub(super) const ALTERNATE_RESOURCE_ID: u32 = AdapterResource::Alternate as u32;
pub(super) const CURSOR_RESOURCE_ID: u32 = AdapterResource::Cursor as u32;
pub(super) const DYNAMIC_RESOURCE_START: u32 = AdapterResource::DynamicStart as u32;

pub(super) fn prepare_cursor(
    request: &mut [u8],
    command: super::CursorCommand,
) -> Result<(), DisplayError> {
    request.fill(0);
    let (opcode, x, y, resource_id, hot_x, hot_y) = match command {
        super::CursorCommand::Move { x, y, visible } => (
            VIRTIO_GPU_CMD_MOVE_CURSOR,
            x,
            y,
            if visible { CURSOR_RESOURCE_ID } else { 0 },
            0,
            0,
        ),
        super::CursorCommand::Update {
            x,
            y,
            visible,
            hot_x,
            hot_y,
        } => (
            VIRTIO_GPU_CMD_UPDATE_CURSOR,
            x,
            y,
            if visible { CURSOR_RESOURCE_ID } else { 0 },
            hot_x,
            hot_y,
        ),
    };
    write_u32(request, 0, opcode).ok_or(DisplayError::Device)?;
    // scanout_id=0 与 padding 已由 fill(0) 固定；cursorq 不设置 controlq fence。
    write_u32(request, 28, x).ok_or(DisplayError::Device)?;
    write_u32(request, 32, y).ok_or(DisplayError::Device)?;
    write_u32(request, 40, resource_id).ok_or(DisplayError::Device)?;
    write_u32(request, 44, hot_x).ok_or(DisplayError::Device)?;
    write_u32(request, 48, hot_y).ok_or(DisplayError::Device)
}

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

pub(super) fn prepare_create_cursor(
    request: &mut [u8],
    resource_id: u32,
) -> Result<(), DisplayError> {
    request.fill(0);
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)?;
    write_u32(request, 28, VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM).ok_or(DisplayError::Device)?;
    write_u32(request, 32, 64).ok_or(DisplayError::Device)?;
    write_u32(request, 36, 64).ok_or(DisplayError::Device)
}

/// ATTACH_BACKING 的 entry 数组：作为请求头之后的第二个 descriptor 发布（Linux
/// `virtio_gpu_cmd_resource_attach_backing` 的 data buffer），entry 数因此不受请求槽大小限制。
pub(super) type AttachEntries = (alloc::sync::Arc<DeviceBacking>, usize);

/// 编码 ATTACH_BACKING：请求头携带 resource 与 entry 数，返回请求长度与 entry 数组。
///
/// `backing` 以设备 BTI 固定，每个设备地址区段是一个 `virtio_gpu_mem_entry`；entry 数组放在一段
/// 物理连续、同样固定的存储中，只占一个 descriptor。
///
/// # Errors
///
/// 固定或分配失败、区段长度超出 u32 返回 `Device`/`InvalidRectangle`。
pub(super) fn prepare_attach(
    request: &mut [u8],
    resource_id: u32,
    backing: &DeviceBacking,
    bti: &Bti,
) -> Result<(usize, AttachEntries), DisplayError> {
    const ENTRY_SIZE: usize = 16;
    let extents = backing.pin(bti).map_err(|_| DisplayError::Device)?;
    request.fill(0);
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)?;
    write_u32(
        request,
        28,
        u32::try_from(extents.len()).map_err(|_| DisplayError::Device)?,
    )
    .ok_or(DisplayError::Device)?;
    let length = extents.len().checked_mul(ENTRY_SIZE).ok_or(DisplayError::Device)?;
    let mut bytes = alloc::vec::Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| DisplayError::Device)?;
    for extent in extents {
        let size = u32::try_from(extent.pages.checked_mul(PAGE_SIZE).ok_or(DisplayError::Device)?)
            .map_err(|_| DisplayError::InvalidRectangle)?;
        bytes.extend_from_slice(&extent.device_address.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
    }
    let entries = DeviceBacking::try_allocate_contiguous(length.div_ceil(PAGE_SIZE), bti).ok_or(DisplayError::Device)?;
    entries.write(0, &bytes).map_err(|_| DisplayError::Device)?;
    entries.pin(bti).map_err(|_| DisplayError::Device)?;
    let entries = alloc::sync::Arc::try_new(entries).map_err(|_| DisplayError::Device)?;
    Ok((32, (entries, length)))
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
