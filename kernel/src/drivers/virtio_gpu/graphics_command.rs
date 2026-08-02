use crate::drivers::{DisplayError, VirglCommand, VirglTransferDirection};

use super::{
    command::PreparedCommand,
    sequence_policy::RuntimeStage,
    wire::{
        CONTROL_REQUEST_SIZE, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_CREATE,
        VIRTIO_GPU_CMD_CTX_DESTROY, VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE,
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, VIRTIO_GPU_CMD_RESOURCE_CREATE_3D,
        VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING, VIRTIO_GPU_CMD_RESOURCE_FLUSH,
        VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SET_SCANOUT, VIRTIO_GPU_CMD_SUBMIT_3D,
        VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D, prepare_attach,
        prepare_flush, prepare_set_scanout, prepare_unref, write_u32, write_u64,
    },
};

/// @description 把 DRM 已验证的 VirGL operation 编码到 adapter-owned DMA request。
/// @param command context/resource ownership 已在 DRM domain 固定的 operation。
/// @param request 唯一 controlq request storage；publication 前由本函数完整覆写。
/// @return opcode、exact request length 与 VirGL completion stage。
/// @errors 非法尺寸、空 identity、未对齐 command stream 或 request 溢出返回稳定 display
/// error。
pub(super) fn prepare(
    command: VirglCommand<'_>,
    request: &mut [u8],
) -> Result<PreparedCommand, DisplayError> {
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
            if resource_id == 0 || width == 0 || height == 0 || depth == 0 || array_size == 0 {
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
            let length = prepare_attach(request, resource_id, &backing)?;
            (VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, length)
        }
        VirglCommand::ResourceDetachBacking { resource_id } => {
            resource_header(request, resource_id)?;
            (VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING, 32)
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
        } => {
            let length = 32usize
                .checked_add(commands.len())
                .filter(|length| *length <= CONTROL_REQUEST_SIZE)
                .ok_or(DisplayError::Device)?;
            if commands.is_empty() || !commands.len().is_multiple_of(4) {
                return Err(DisplayError::Device);
            }
            context_header(request, context_id, length)?;
            write_u32(request, 24, commands.len() as u32).ok_or(DisplayError::Device)?;
            request[32..length].copy_from_slice(commands);
            (VIRTIO_GPU_CMD_SUBMIT_3D, length)
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

fn resource_header(request: &mut [u8], resource_id: u32) -> Result<(), DisplayError> {
    if resource_id == 0 {
        return Err(DisplayError::Device);
    }
    clear(request, 32)?;
    write_u32(request, 24, resource_id).ok_or(DisplayError::Device)
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
    use super::*;
    use crate::drivers::VirglBox;

    #[test]
    fn submit_encodes_context_and_exact_dword_stream() {
        let mut request = [0xff; CONTROL_REQUEST_SIZE];
        let commands = [1, 2, 3, 4, 5, 6, 7, 8];
        let prepared = prepare(
            VirglCommand::Submit3d {
                context_id: 9,
                commands: &commands,
            },
            &mut request,
        )
        .unwrap();
        assert_eq!(prepared.opcode, VIRTIO_GPU_CMD_SUBMIT_3D);
        assert_eq!(prepared.length, 40);
        assert_eq!(&request[16..20], &9u32.to_le_bytes());
        assert_eq!(&request[24..28], &8u32.to_le_bytes());
        assert_eq!(&request[32..40], &commands);
    }

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
