use alloc::sync::Arc;

use crate::{
    drivers::{DisplayError, DisplayMode, DisplayRect},
    memory::DeviceBacking,
};

use super::{
    sequence_policy::RuntimeStage,
    wire::{
        CONTROL_HEADER_SIZE, DISPLAY_INFO_SIZE, VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
        VIRTIO_GPU_CMD_RESOURCE_FLUSH, VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SET_SCANOUT,
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
        VIRTIO_GPU_RESP_OK_NODATA, prepare_attach, prepare_create, prepare_flush,
        prepare_set_scanout, prepare_transfer, prepare_unref,
    },
};

/// RESOURCE_UNREF 在 transaction 中承担的唯一领域目的。
pub(super) enum UnrefPurpose {
    Evicted,
    Boot,
    Released,
    Disabled(u8),
}

/// SET_SCANOUT command 完成后应进入的唯一阶段。
pub(super) enum ScanoutPurpose {
    Activate,
    Disable,
}

/// RESOURCE_FLUSH command 完成后应提交的 transaction 结果。
pub(super) enum FlushPurpose {
    Scanout,
    Damage,
}

/// 一条尚未编码、但已绑定 opcode/长度/next-stage 的领域 command。
pub(super) enum GpuCommand {
    DisplayInfo,
    Create {
        mode: DisplayMode,
        resource_id: u32,
    },
    Attach {
        resource_id: u32,
        backing: Arc<DeviceBacking>,
    },
    TransferScanout {
        mode: DisplayMode,
        resource_id: u32,
    },
    SetScanout {
        mode: DisplayMode,
        resource_id: u32,
        purpose: ScanoutPurpose,
    },
    Flush {
        rectangle: DisplayRect,
        resource_id: u32,
        purpose: FlushPurpose,
    },
    Unref {
        resource_id: u32,
        purpose: UnrefPurpose,
    },
}

/// 编码完成、可由唯一 queue publication seam 提交的 command proof。
pub(super) struct PreparedCommand {
    pub(super) opcode: u32,
    pub(super) length: usize,
    pub(super) stage: RuntimeStage,
}

/// controlq 中唯一在途 command 的 descriptor 与 fence 凭据。
pub(super) struct PendingCommand {
    pub(super) head: u16,
    pub(super) operation_fence: u64,
    pub(super) command_fence: u64,
    pub(super) stage: RuntimeStage,
}

impl PendingCommand {
    /// @description 返回当前 stage 唯一合法的 device-written response 长度。
    pub(super) const fn response_length(&self) -> usize {
        if matches!(self.stage, RuntimeStage::DisplayInfo) {
            DISPLAY_INFO_SIZE
        } else {
            CONTROL_HEADER_SIZE
        }
    }
}

impl GpuCommand {
    /// 返回该领域 command 完成时唯一合法的 runtime stage。
    pub(super) const fn stage(&self) -> RuntimeStage {
        match self {
            Self::DisplayInfo => RuntimeStage::DisplayInfo,
            Self::Create { .. } => RuntimeStage::Create,
            Self::Attach { .. } => RuntimeStage::Attach,
            Self::TransferScanout { .. } => RuntimeStage::TransferScanout,
            Self::SetScanout { purpose, .. } => match purpose {
                ScanoutPurpose::Activate => RuntimeStage::SetScanout,
                ScanoutPurpose::Disable => RuntimeStage::DisableScanout,
            },
            Self::Flush { purpose, .. } => match purpose {
                FlushPurpose::Scanout => RuntimeStage::FlushScanout,
                FlushPurpose::Damage => RuntimeStage::FlushDamage,
            },
            Self::Unref { purpose, .. } => match purpose {
                UnrefPurpose::Evicted => RuntimeStage::UnrefEvicted,
                UnrefPurpose::Boot => RuntimeStage::UnrefBoot,
                UnrefPurpose::Released => RuntimeStage::UnrefReleased,
                UnrefPurpose::Disabled(slot) => RuntimeStage::UnrefDisabled(*slot),
            },
        }
    }

    /// 编码 wire payload，并一次返回与其不可分离的 opcode、长度和 next-stage。
    pub(super) fn prepare(self, request: &mut [u8]) -> Result<PreparedCommand, DisplayError> {
        let stage = self.stage();
        let (opcode, length) = match self {
            Self::DisplayInfo => {
                request.fill(0);
                (VIRTIO_GPU_CMD_GET_DISPLAY_INFO, CONTROL_HEADER_SIZE)
            }
            Self::Create { mode, resource_id } => {
                prepare_create(request, mode, resource_id)?;
                (VIRTIO_GPU_CMD_RESOURCE_CREATE_2D, 40)
            }
            Self::Attach {
                resource_id,
                backing,
            } => (
                VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                prepare_attach(request, resource_id, &backing)?,
            ),
            Self::TransferScanout { mode, resource_id } => {
                prepare_transfer(
                    request,
                    mode,
                    DisplayRect {
                        x: 0,
                        y: 0,
                        width: mode.width,
                        height: mode.height,
                    },
                    resource_id,
                )?;
                (VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, 56)
            }
            Self::SetScanout {
                mode, resource_id, ..
            } => {
                prepare_set_scanout(request, mode, resource_id)?;
                (VIRTIO_GPU_CMD_SET_SCANOUT, 48)
            }
            Self::Flush {
                rectangle,
                resource_id,
                ..
            } => {
                prepare_flush(request, rectangle, resource_id)?;
                (VIRTIO_GPU_CMD_RESOURCE_FLUSH, 48)
            }
            Self::Unref { resource_id, .. } => {
                prepare_unref(request, resource_id)?;
                (VIRTIO_GPU_CMD_RESOURCE_UNREF, 32)
            }
        };
        Ok(PreparedCommand {
            opcode,
            length,
            stage,
        })
    }
}

impl RuntimeStage {
    /// 返回该 stage 唯一允许的 fenced response type。
    pub(super) const fn expected_response(self) -> u32 {
        match self {
            Self::DisplayInfo => VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
            _ => VIRTIO_GPU_RESP_OK_NODATA,
        }
    }
}
