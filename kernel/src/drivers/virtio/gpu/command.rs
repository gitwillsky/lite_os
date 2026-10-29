use super::wire::{VIRTIO_GPU_FLAG_FENCE, read_u32, read_u64};
use super::{
    CAPSET_RESPONSE_SIZE, CONTROL_COMMAND_CAPACITY, CONTROL_REQUEST_SIZE, VirtIOGpuDevice,
    sequence_policy,
};
use crate::drivers::virtio::queue::DmaBuffer;
use alloc::{sync::Arc, vec::Vec};

use crate::{
    devices::model::{DisplayError, DisplayMode, DisplayRect},
    memory::DeviceBacking,
};

use super::{
    sequence_policy::RuntimeStage,
    wire::{
        CONTROL_HEADER_SIZE, DISPLAY_INFO_SIZE, VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
        VIRTIO_GPU_CMD_RESOURCE_FLUSH, VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SET_SCANOUT,
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
        VIRTIO_GPU_RESP_OK_NODATA, prepare_attach, prepare_create, prepare_create_cursor,
        prepare_flush, prepare_set_scanout, prepare_transfer, prepare_unref,
    },
};

/// RESOURCE_UNREF 在 transaction 中承担的唯一领域目的。
pub(super) enum UnrefPurpose {
    Evicted,
    Boot,
    Cursor,
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
    CreateCursor {
        resource_id: u32,
    },
    AttachCursor {
        resource_id: u32,
        backing: Arc<DeviceBacking>,
    },
    TransferCursor {
        resource_id: u32,
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
    /// SUBMIT_3D 的命令流 backing 与字节数；作为请求头之后的第二个 descriptor 发布。
    pub(super) stream: Option<(Arc<DeviceBacking>, usize)>,
}

/// controlq 一个有界 slot 中的 descriptor 与 fence 凭据。
pub(super) struct PendingCommand {
    pub(super) head: u16,
    pub(super) operation_fence: u64,
    pub(super) command_fence: u64,
    pub(super) stage: RuntimeStage,
}

impl PendingCommand {
    /// 返回当前 stage 唯一合法的 device-written response 长度。
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
            Self::CreateCursor { .. } => RuntimeStage::CreateCursor,
            Self::AttachCursor { .. } => RuntimeStage::AttachCursor,
            Self::TransferCursor { .. } => RuntimeStage::TransferCursor,
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
                UnrefPurpose::Cursor => RuntimeStage::UnrefCursor,
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
            Self::CreateCursor { resource_id } => {
                prepare_create_cursor(request, resource_id)?;
                (VIRTIO_GPU_CMD_RESOURCE_CREATE_2D, 40)
            }
            Self::AttachCursor {
                resource_id,
                backing,
            } => (
                VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                prepare_attach(request, resource_id, &backing)?,
            ),
            Self::TransferCursor { resource_id } => {
                let mode = DisplayMode {
                    width: 64,
                    height: 64,
                    pitch: 64 * 4,
                };
                prepare_transfer(
                    request,
                    mode,
                    DisplayRect {
                        x: 0,
                        y: 0,
                        width: 64,
                        height: 64,
                    },
                    resource_id,
                )?;
                (VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, 56)
            }
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
            stream: None,
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

pub(super) struct CommandSlot {
    pub(super) request: DmaBuffer<CONTROL_REQUEST_SIZE>,
    pub(super) response: DmaBuffer<CAPSET_RESPONSE_SIZE>,
    pub(super) pending: Option<PendingCommand>,
    /// 在途 SUBMIT_3D 的命令流 backing；设备经 used ring 归还 descriptor 前不得释放，
    /// 否则设备会读到已回收的物理页。
    pub(super) stream: Option<Arc<DeviceBacking>>,
}

pub(super) struct CommandSlots {
    pub(super) slots: Vec<CommandSlot>,
}

pub(super) enum CommandPayload {
    None,
    DisplayInfo(DisplayMode),
}

pub(super) struct CompletedCommand {
    pending: PendingCommand,
    payload: CommandPayload,
    stream: Option<Arc<DeviceBacking>>,
}

impl CompletedCommand {
    /// 交出已完成 SUBMIT_3D 的命令流 backing；调用方必须在释放控制队列锁之后再析构它。
    pub(super) fn take_stream(&mut self) -> Option<Arc<DeviceBacking>> {
        self.stream.take()
    }

    pub(super) fn into_parts(self) -> (PendingCommand, CommandPayload) {
        (self.pending, self.payload)
    }
}

impl CommandSlots {
    pub(super) fn try_new() -> Option<Self> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(CONTROL_COMMAND_CAPACITY).ok()?;
        for _ in 0..CONTROL_COMMAND_CAPACITY {
            slots.push(CommandSlot {
                request: DmaBuffer::try_zeroed().ok()?,
                response: DmaBuffer::try_zeroed().ok()?,
                pending: None,
                stream: None,
            });
        }
        Some(Self { slots })
    }

    pub(super) fn idle_index(&self) -> Option<usize> {
        self.slots.iter().position(|slot| slot.pending.is_none())
    }

    pub(super) fn has_pending(&self) -> bool {
        self.slots.iter().any(|slot| slot.pending.is_some())
    }

    pub(super) fn has_non_render_pending(&self) -> bool {
        self.slots.iter().any(|slot| {
            slot.pending.as_ref().is_some_and(|pending| {
                !matches!(
                    pending.stage,
                    sequence_policy::RuntimeStage::Virgl
                        | sequence_policy::RuntimeStage::VirglSetScanout
                        | sequence_policy::RuntimeStage::VirglFlush
                )
            })
        })
    }

    pub(super) fn idle_pair(&self) -> Option<[usize; 2]> {
        let mut idle = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.pending.is_none().then_some(index));
        Some([idle.next()?, idle.next()?])
    }

    pub(super) fn request_mut(&mut self, index: usize) -> &mut DmaBuffer<CONTROL_REQUEST_SIZE> {
        assert!(self.slots[index].pending.is_none());
        &mut self.slots[index].request
    }

    pub(super) fn complete(
        &mut self,
        head: u16,
        length: usize,
    ) -> Result<CompletedCommand, DisplayError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| {
                slot.pending
                    .as_ref()
                    .is_some_and(|pending| pending.head == head)
            })
            .ok_or(DisplayError::Device)?;
        let pending = slot.pending.as_ref().ok_or(DisplayError::Device)?;
        if length != pending.response_length()
            || read_u32(slot.response.as_slice(), 0) != Some(pending.stage.expected_response())
            || read_u32(slot.response.as_slice(), 4)
                .is_none_or(|flags| flags & VIRTIO_GPU_FLAG_FENCE == 0)
            || read_u64(slot.response.as_slice(), 8) != Some(pending.command_fence)
        {
            // 在 fail-stop 前保留被拒绝命令的 wire 身份，不能只向 DRM 返回丢失上下文的 Device。
            error!(
                "VirtIO GPU response mismatch: head={} opcode={:#x} context={} arg0={:#x} stage={:?} length={}/{} response={:#x} flags={:#x} fence={}/{}",
                head,
                read_u32(slot.request.as_slice(), 0).unwrap_or(0),
                read_u32(slot.request.as_slice(), 16).unwrap_or(0),
                read_u32(slot.request.as_slice(), 24).unwrap_or(0),
                pending.stage,
                length,
                pending.response_length(),
                read_u32(slot.response.as_slice(), 0).unwrap_or(0),
                read_u32(slot.response.as_slice(), 4).unwrap_or(0),
                read_u64(slot.response.as_slice(), 8).unwrap_or(0),
                pending.command_fence,
            );
            return Err(DisplayError::Device);
        }
        let payload = match pending.stage {
            sequence_policy::RuntimeStage::DisplayInfo => CommandPayload::DisplayInfo(
                VirtIOGpuDevice::parse_display_mode(slot.response.as_slice())
                    .ok_or(DisplayError::Device)?,
            ),
            _ => CommandPayload::None,
        };
        let pending = slot.pending.take().ok_or(DisplayError::Device)?;
        Ok(CompletedCommand {
            pending,
            payload,
            stream: slot.stream.take(),
        })
    }

    pub(super) fn boot_slot_mut(&mut self) -> &mut CommandSlot {
        assert!(!self.has_pending());
        &mut self.slots[0]
    }

    pub(super) fn boot_response(&self) -> &DmaBuffer<CAPSET_RESPONSE_SIZE> {
        assert!(!self.has_pending());
        &self.slots[0].response
    }
}
