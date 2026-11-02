use alloc::sync::Arc;
use device_model::{DeviceBacking, DisplayError, VirglCommand, VirglTransferDirection};
use native_runtime::native_abi::PAGE_SIZE;

use super::{
    command::PreparedCommand,
    sequence_policy::RuntimeStage,
    wire::{
        VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_CREATE, VIRTIO_GPU_CMD_CTX_DESTROY,
        VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE, VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
        VIRTIO_GPU_CMD_RESOURCE_CREATE_3D, VIRTIO_GPU_CMD_RESOURCE_FLUSH,
        VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SET_SCANOUT, VIRTIO_GPU_CMD_SUBMIT_3D,
        VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D, prepare_attach,
        prepare_flush, prepare_set_scanout, prepare_unref, write_u32, write_u64,
    },
};

/// 把 DRM 已验证的 VirGL operation 编码到 adapter-owned DMA request。
///
/// # Parameters
///
/// - `command`: context/resource ownership 已在 DRM domain 固定的 operation。
/// - `request`: 唯一 controlq request storage；publication 前由本函数完整覆写。
/// - `descriptor_capacity`: 已协商的 controlq 大小；超出一条完整 chain 时在设备侧整理 DMA backing。
///
/// # Returns
///
/// opcode、exact request length 与 VirGL completion stage。
///
/// # Errors
///
/// 非法尺寸、空 identity、未对齐 command stream 或 request 溢出返回稳定 display
/// error。
pub(super) fn prepare(
    command: VirglCommand<'_>,
    request: &mut [u8],
    bti: &native_runtime::Bti,
    descriptor_capacity: usize,
) -> Result<PreparedCommand, DisplayError> {
    let mut stream = None;
    let (opcode, length) = match command {
        VirglCommand::ContextCreate {
            context_id,
            context_init,
            name,
        } => {
            if context_id == 0 || name.len() > 64 {
                return Err(DisplayError::Device);
            }
            clear(request, 96)?;
            write_u32(request, 16, context_id).ok_or(DisplayError::Device)?;
            write_u32(request, 24, name.len() as u32).ok_or(DisplayError::Device)?;
            write_u32(request, 28, context_init).ok_or(DisplayError::Device)?;
            request[32..32 + name.len()].copy_from_slice(name);
            (VIRTIO_GPU_CMD_CTX_CREATE, 96)
        }
        VirglCommand::ContextDestroy { context_id } => {
            context_header(request, context_id, 24)?;
            (VIRTIO_GPU_CMD_CTX_DESTROY, 24)
        }
        VirglCommand::ResourceCreate3d {
            resource_id,
            target,
            format,
            bind,
            width,
            height,
            depth,
            array_size,
            last_level,
            samples,
            flags,
        } => {
            if resource_id == 0
                || width == 0
                || height == 0
                || depth == 0
                || (array_size == 0 && target != 0)
            {
                return Err(DisplayError::InvalidRectangle);
            }
            clear(request, 72)?;
            for (offset, value) in [
                (24, resource_id),
                (28, target),
                (32, format),
                (36, bind),
                (40, width),
                (44, height),
                (48, depth),
                (52, array_size),
                (56, last_level),
                (60, samples),
                (64, flags),
            ] {
                write_u32(request, offset, value).ok_or(DisplayError::Device)?;
            }
            (VIRTIO_GPU_CMD_RESOURCE_CREATE_3D, 72)
        }
        VirglCommand::ResourceAttachBacking {
            resource_id,
            backing,
        } => {
            if resource_id == 0 {
                return Err(DisplayError::Device);
            }
            let (length, entries) = prepare_attach(request, resource_id, &backing, bti)?;
            stream = Some(entries);
            (VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, length)
        }
        VirglCommand::ContextAttachResource {
            context_id,
            resource_id,
        } => {
            context_resource(request, context_id, resource_id)?;
            (VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, 32)
        }
        VirglCommand::ContextDetachResource {
            context_id,
            resource_id,
        } => {
            context_resource(request, context_id, resource_id)?;
            (VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE, 32)
        }
        VirglCommand::Submit3d {
            context_id,
            commands,
            length,
        } => {
            // 请求只含 32 字节头；命令流作为 chain 的第二个 descriptor 直接从 backing 读取。
            if length == 0
                || !length.is_multiple_of(4)
                || length > commands.pages().saturating_mul(PAGE_SIZE)
            {
                return Err(DisplayError::Device);
            }
            context_header(request, context_id, 32)?;
            write_u32(
                request,
                24,
                u32::try_from(length).map_err(|_| DisplayError::Device)?,
            )
            .ok_or(DisplayError::Device)?;
            // 1. 命令流由设备经 DMA 读取；片段数取决于本次 BTI 映射，不能把物理碎片数当作 UAPI 限制。
            let mut remaining = length;
            let mut descriptors = 2;
            for extent in commands.pin(bti).map_err(|_| DisplayError::Device)? {
                if remaining == 0 {
                    break;
                }
                descriptors += 1;
                remaining = remaining.saturating_sub(extent.pages * PAGE_SIZE);
            }
            // 2. 为请求头与响应各留一个 descriptor。整条 chain 超出队列时必须在 publication 前
            //    整理 DMA 存储，否则即使队列全空也无法提交，重试只会永久等待。
            let commands = if descriptors > descriptor_capacity {
                let contiguous =
                    DeviceBacking::try_allocate_contiguous(length.div_ceil(PAGE_SIZE), bti)
                        .ok_or(DisplayError::Device)?;
                let source = commands.cpu_address().ok_or(DisplayError::Device)?;
                let destination = contiguous.cpu_address().ok_or(DisplayError::Device)?;
                // SAFETY: 两个独立 VMO 的映射覆盖上面验证的 length，源命令在提交期间不可变；
                // 目标尚未发布给设备、没有其他访问者，且不与源映射重叠。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        source as *const u8,
                        destination as *mut u8,
                        length,
                    );
                }
                contiguous.pin(bti).map_err(|_| DisplayError::Device)?;
                Arc::try_new(contiguous).map_err(|_| DisplayError::Device)?
            } else {
                commands
            };
            // 3. prepared/slot 独占最终 backing，沿已有 exact fence 保活到 used-ring 完成；
            //    错误路径尚未发布，原始与整理后的 VMO/PMT 都由 RAII 回收。
            stream = Some((commands, length));
            (VIRTIO_GPU_CMD_SUBMIT_3D, 32)
        }
        VirglCommand::Transfer3d {
            direction,
            context_id,
            offset,
            resource_id,
            level,
            region,
            stride,
            layer_stride,
        } => {
            if resource_id == 0 || region.width == 0 || region.height == 0 || region.depth == 0 {
                return Err(DisplayError::InvalidRectangle);
            }
            context_header(request, context_id, 72)?;
            for (field_offset, value) in [
                (24, region.x),
                (28, region.y),
                (32, region.z),
                (36, region.width),
                (40, region.height),
                (44, region.depth),
            ] {
                write_u32(request, field_offset, value).ok_or(DisplayError::Device)?;
            }
            write_u64(request, 48, offset).ok_or(DisplayError::Device)?;
            write_u32(request, 56, resource_id).ok_or(DisplayError::Device)?;
            write_u32(request, 60, level).ok_or(DisplayError::Device)?;
            write_u32(request, 64, stride).ok_or(DisplayError::Device)?;
            write_u32(request, 68, layer_stride).ok_or(DisplayError::Device)?;
            let opcode = match direction {
                VirglTransferDirection::ToHost => VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D,
                VirglTransferDirection::FromHost => VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D,
            };
            (opcode, 72)
        }
        VirglCommand::SetScanout { mode, resource_id } => {
            prepare_set_scanout(request, mode, resource_id)?;
            (VIRTIO_GPU_CMD_SET_SCANOUT, 48)
        }
        VirglCommand::Flush {
            resource_id,
            rectangle,
        } => {
            if resource_id == 0 {
                return Err(DisplayError::Device);
            }
            prepare_flush(request, rectangle, resource_id)?;
            (VIRTIO_GPU_CMD_RESOURCE_FLUSH, 48)
        }
        VirglCommand::ResourceUnref { resource_id } => {
            if resource_id == 0 {
                return Err(DisplayError::Device);
            }
            prepare_unref(request, resource_id)?;
            (VIRTIO_GPU_CMD_RESOURCE_UNREF, 32)
        }
    };
    Ok(PreparedCommand {
        opcode,
        length,
        stage: RuntimeStage::Virgl,
        stream,
    })
}

fn clear(request: &mut [u8], length: usize) -> Result<(), DisplayError> {
    request
        .get_mut(..length)
        .ok_or(DisplayError::Device)?
        .fill(0);
    Ok(())
}

fn context_header(request: &mut [u8], context_id: u32, length: usize) -> Result<(), DisplayError> {
    if context_id == 0 {
        return Err(DisplayError::Device);
    }
    clear(request, length)?;
    write_u32(request, 16, context_id).ok_or(DisplayError::Device)
}

fn context_resource(
    request: &mut [u8],
    context_id: u32,
    resource_id: u32,
) -> Result<(), DisplayError> {
    if resource_id == 0 {
        return Err(DisplayError::Device);
    }
    context_header(request, context_id, 32)?;
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)
}

#[cfg(test)]
mod tests {
    use super::super::wire::CONTROL_REQUEST_SIZE;
    use super::*;
    use device_model::VirglBox;

    #[test]
    fn transfer_3d_matches_virtio_wire_offsets() {
        let mut request = [0; CONTROL_REQUEST_SIZE];
        let prepared = prepare(
            VirglCommand::Transfer3d {
                direction: VirglTransferDirection::FromHost,
                context_id: 3,
                offset: 0x1122_3344_5566_7788,
                resource_id: 7,
                level: 2,
                region: VirglBox {
                    x: 1,
                    y: 2,
                    z: 3,
                    width: 4,
                    height: 5,
                    depth: 6,
                },
                stride: 256,
                layer_stride: 4096,
            },
            &mut request,
        )
        .unwrap();
        assert_eq!(prepared.opcode, VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D);
        assert_eq!(prepared.length, 72);
        assert_eq!(&request[48..56], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&request[56..60], &7u32.to_le_bytes());
        assert_eq!(&request[64..68], &256u32.to_le_bytes());
    }
}
