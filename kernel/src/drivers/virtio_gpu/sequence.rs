use super::{
    CommandPayload, CompletedCommand, ControlQueue,
    command::{FlushPurpose, GpuCommand, PendingCommand, ScanoutPurpose, UnrefPurpose},
    resource::{
        CursorResource, ResidentResource, ResourceRelease, ResourceSnapshot, RuntimeOperation,
        disabled_resource, full_rectangle, operation_target, operation_target_ref,
    },
    sequence_policy::RuntimeStage,
};
use crate::drivers::{DisplayError, DisplayMode, DisplayUpdate};

/// completion 状态机选出的唯一后继动作；wire encoding 与 queue publication 不在分支内发生。
pub(super) enum SequenceAction {
    Command {
        command: GpuCommand,
        operation_fence: u64,
    },
    DamageBatch {
        operation_fence: u64,
        mode: DisplayMode,
        resource_id: u32,
    },
    Finished(SequenceCompletion),
}

/// 必须在 control lock 释放后才析构的 resource owner。
pub(super) enum SequenceRetirement {
    None,
    Resident(Option<ResidentResource>),
    Boot {
        boot: ResourceRelease,
        evicted: Option<ResidentResource>,
    },
    Released(ResourceRelease),
    Disabled(ResourceSnapshot),
    Cursor(Option<CursorResource>),
}

impl SequenceRetirement {
    /// 在 control lock 释放后析构可能归还大量 framebuffer extent 的 owner。
    pub(super) fn release_after_unlock(self) {
        match self {
            Self::None => {}
            Self::Resident(resource) => drop(resource),
            Self::Boot { boot, evicted } => {
                drop(boot);
                drop(evicted);
            }
            Self::Released(resource) => drop(resource),
            Self::Disabled(resources) => drop(resources),
            Self::Cursor(resource) => drop(resource),
        }
    }
}

/// 一次 command sequence 的用户可见结果与延迟析构 owner。
pub(super) struct SequenceCompletion {
    pub(super) update: Option<DisplayUpdate>,
    pub(super) retirement: SequenceRetirement,
}

impl SequenceCompletion {
    fn update(update: Option<DisplayUpdate>) -> SequenceAction {
        SequenceAction::Finished(Self {
            update,
            retirement: SequenceRetirement::None,
        })
    }

    fn operation(fence: u64, retirement: SequenceRetirement) -> SequenceAction {
        SequenceAction::Finished(Self {
            update: Some(DisplayUpdate::OperationCompleted(fence)),
            retirement,
        })
    }
}

fn command_after(
    previous: RuntimeStage,
    operation_fence: u64,
    command: GpuCommand,
) -> Result<SequenceAction, DisplayError> {
    previous
        .validate_successor(command.stage())
        .map_err(|_| DisplayError::Device)?;
    Ok(SequenceAction::Command {
        command,
        operation_fence,
    })
}

/// 验证一个 used completion，并只选择下一条领域 command 或 terminal result。
pub(super) fn complete(
    control: &mut ControlQueue,
    completed: CompletedCommand,
) -> Result<SequenceAction, DisplayError> {
    let (pending, payload) = completed.into_parts();
    let fence = pending.operation_fence;
    match pending.stage {
        RuntimeStage::DisplayInfo => {
            let CommandPayload::DisplayInfo(mode) = payload else {
                return Err(DisplayError::Device);
            };
            let update = if mode != control.mode {
                DisplayUpdate::ModeChanged(mode)
            } else {
                DisplayUpdate::AdapterReady
            };
            control.mode = mode;
            Ok(SequenceCompletion::update(Some(update)))
        }
        RuntimeStage::UnrefEvicted => {
            let (mode, resource_id) = operation_target(&control.operation, &control.resources)?;
            command_after(
                pending.stage,
                fence,
                GpuCommand::Create { mode, resource_id },
            )
        }
        RuntimeStage::Create => {
            let resource_id = operation_target(&control.operation, &control.resources)?.1;
            let backing =
                operation_target_ref(&control.operation)?.backing_owner(&control.resources);
            command_after(
                pending.stage,
                fence,
                GpuCommand::Attach {
                    resource_id,
                    backing,
                },
            )
        }
        RuntimeStage::Attach => {
            let (mode, resource_id) = operation_target(&control.operation, &control.resources)?;
            if matches!(
                control.operation.as_ref(),
                Some(RuntimeOperation::Damage(_))
            ) {
                Ok(SequenceAction::DamageBatch {
                    operation_fence: fence,
                    mode,
                    resource_id,
                })
            } else {
                command_after(
                    pending.stage,
                    fence,
                    GpuCommand::TransferScanout { mode, resource_id },
                )
            }
        }
        RuntimeStage::UnrefCursor => {
            let target = match control.operation.as_ref() {
                Some(RuntimeOperation::CursorUpload(target)) => target,
                _ => return Err(DisplayError::Device),
            };
            command_after(
                pending.stage,
                fence,
                GpuCommand::CreateCursor {
                    resource_id: target.id(),
                },
            )
        }
        RuntimeStage::CreateCursor => {
            let target = match control.operation.as_ref() {
                Some(RuntimeOperation::CursorUpload(target)) => target,
                _ => return Err(DisplayError::Device),
            };
            command_after(
                pending.stage,
                fence,
                GpuCommand::AttachCursor {
                    resource_id: target.id(),
                    backing: target.backing_owner(&control.cursor_resource),
                },
            )
        }
        RuntimeStage::AttachCursor => {
            let target = match control.operation.as_ref() {
                Some(RuntimeOperation::CursorUpload(target)) => target,
                _ => return Err(DisplayError::Device),
            };
            command_after(
                pending.stage,
                fence,
                GpuCommand::TransferCursor {
                    resource_id: target.id(),
                },
            )
        }
        RuntimeStage::TransferCursor => {
            let target = match control.operation.take() {
                Some(RuntimeOperation::CursorUpload(target)) => target,
                _ => return Err(DisplayError::Device),
            };
            let evicted = control.cursor_resource.complete(target);
            Ok(SequenceAction::Finished(SequenceCompletion {
                update: Some(DisplayUpdate::RenderCompleted(fence)),
                retirement: SequenceRetirement::Cursor(evicted),
            }))
        }
        RuntimeStage::TransferScanout => {
            let (mode, resource_id) = operation_target(&control.operation, &control.resources)?;
            if !matches!(
                control.operation.as_ref(),
                Some(RuntimeOperation::Scanout(_))
            ) {
                return Err(DisplayError::Device);
            }
            command_after(
                pending.stage,
                fence,
                GpuCommand::SetScanout {
                    mode,
                    resource_id,
                    purpose: ScanoutPurpose::Activate,
                },
            )
        }
        RuntimeStage::SetScanout => {
            let (mode, resource_id) = operation_target(&control.operation, &control.resources)?;
            if !matches!(
                control.operation.as_ref(),
                Some(RuntimeOperation::Scanout(_))
            ) {
                return Err(DisplayError::Device);
            }
            command_after(
                pending.stage,
                fence,
                GpuCommand::Flush {
                    rectangle: full_rectangle(mode),
                    resource_id,
                    purpose: FlushPurpose::Scanout,
                },
            )
        }
        RuntimeStage::FlushScanout => finish_scanout(control, pending),
        RuntimeStage::UnrefBoot => {
            let (boot, evicted) = match control.operation.take() {
                Some(RuntimeOperation::RetireBoot { boot, evicted }) => (boot, evicted),
                _ => return Err(DisplayError::Device),
            };
            Ok(SequenceCompletion::operation(
                fence,
                SequenceRetirement::Boot { boot, evicted },
            ))
        }
        RuntimeStage::FlushDamage => {
            let target = match control.operation.take() {
                Some(RuntimeOperation::Damage(target)) => target,
                _ => return Err(DisplayError::Device),
            };
            let evicted = control.resources.complete(target, false, true);
            Ok(SequenceCompletion::operation(
                fence,
                SequenceRetirement::Resident(evicted),
            ))
        }
        RuntimeStage::UnrefReleased => {
            let release = match control.operation.take() {
                Some(RuntimeOperation::Release(release)) => release,
                _ => return Err(DisplayError::Device),
            };
            Ok(SequenceCompletion::operation(
                fence,
                SequenceRetirement::Released(release),
            ))
        }
        RuntimeStage::DisableScanout => unref_disabled(control, pending, 0),
        RuntimeStage::UnrefDisabled(slot) => {
            unref_disabled(control, pending, usize::from(slot) + 1)
        }
        RuntimeStage::Virgl => match payload {
            CommandPayload::None => Ok(SequenceCompletion::update(Some(
                DisplayUpdate::RenderCompleted(fence),
            ))),
            _ => Err(DisplayError::Device),
        },
        // SET_SCANOUT 只改变 host scanout 绑定；同一批次中后继的 RESOURCE_FLUSH 才触发
        // dpy_gl_update。若在这里发布 fence，客体会把尚未可见的黑帧误报为已呈现。
        RuntimeStage::VirglSetScanout => match payload {
            CommandPayload::None => Ok(SequenceCompletion::update(None)),
            _ => Err(DisplayError::Device),
        },
        RuntimeStage::VirglFlush => match payload {
            CommandPayload::None => Ok(SequenceCompletion::update(Some(
                DisplayUpdate::RenderCompleted(fence),
            ))),
            _ => Err(DisplayError::Device),
        },
    }
}

fn finish_scanout(
    control: &mut ControlQueue,
    pending: PendingCommand,
) -> Result<SequenceAction, DisplayError> {
    let target = match control.operation.take() {
        Some(RuntimeOperation::Scanout(target)) => target,
        _ => return Err(DisplayError::Device),
    };
    let evicted = control.resources.complete(target, true, false);
    if let Some(boot) = control.resources.release(0)? {
        let resource_id = boot.id();
        control.operation = Some(RuntimeOperation::RetireBoot { boot, evicted });
        command_after(
            pending.stage,
            pending.operation_fence,
            GpuCommand::Unref {
                resource_id,
                purpose: UnrefPurpose::Boot,
            },
        )
    } else {
        Ok(SequenceCompletion::operation(
            pending.operation_fence,
            SequenceRetirement::Resident(evicted),
        ))
    }
}

fn unref_disabled(
    control: &mut ControlQueue,
    pending: PendingCommand,
    start: usize,
) -> Result<SequenceAction, DisplayError> {
    if let Some((next, resource_id)) = disabled_resource(&control.operation, start)? {
        command_after(
            pending.stage,
            pending.operation_fence,
            GpuCommand::Unref {
                resource_id,
                purpose: UnrefPurpose::Disabled(next as u8),
            },
        )
    } else {
        let resources = match control.operation.take() {
            Some(RuntimeOperation::Disable(resources)) => resources,
            _ => return Err(DisplayError::Device),
        };
        Ok(SequenceCompletion::operation(
            pending.operation_fence,
            SequenceRetirement::Disabled(resources),
        ))
    }
}

/// damage batch 全部完成后选择下一批，或唯一最终 FLUSH command。
pub(super) fn finish_damage_batch(
    control: &mut ControlQueue,
) -> Result<SequenceAction, DisplayError> {
    let operation_fence = control.damage.operation_fence();
    control.damage.finish_batch();
    let (mode, resource_id) = operation_target(&control.operation, &control.resources)?;
    if control.damage.has_remaining() {
        Ok(SequenceAction::DamageBatch {
            operation_fence,
            mode,
            resource_id,
        })
    } else {
        Ok(SequenceAction::Command {
            command: GpuCommand::Flush {
                rectangle: control.damage.flush_rectangle(),
                resource_id,
                purpose: FlushPurpose::Damage,
            },
            operation_fence,
        })
    }
}
