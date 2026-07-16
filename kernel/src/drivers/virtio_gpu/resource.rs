use alloc::sync::Arc;

use crate::{
    drivers::{DisplayError, DisplayMode},
    memory::{DeviceBacking, PAGE_SIZE},
};

use super::wire::{ALTERNATE_RESOURCE_ID, BOOT_RESOURCE_ID};
use super::{VIRTIO_GPU_CMD_RESOURCE_UNREF, VirtIOGpuDevice, prepare_unref};

const RESOURCE_IDS: [u32; 2] = [BOOT_RESOURCE_ID, ALTERNATE_RESOURCE_ID];

/// @description 验证 framebuffer mode 可由给定 SG backing 完整覆盖。
/// @param mode framebuffer 的 canonical linear mode。
/// @param backing 在 device operation 完成前保持存活的 SG owner。
/// @return backing 容量和 VirtIO 32-bit length 均合法时返回成功。
/// @errors pitch×height 溢出、超过 backing 或超过 VirtIO length 时返回 InvalidRectangle。
pub(super) fn validate_backing(
    mode: DisplayMode,
    backing: &DeviceBacking,
) -> Result<(), DisplayError> {
    let bytes = usize::try_from(mode.pitch)
        .ok()
        .and_then(|pitch| pitch.checked_mul(mode.height as usize))
        .ok_or(DisplayError::InvalidRectangle)?;
    if backing
        .pages()
        .checked_mul(PAGE_SIZE)
        .is_none_or(|capacity| capacity < bytes)
        || u32::try_from(bytes).is_err()
    {
        return Err(DisplayError::InvalidRectangle);
    }
    Ok(())
}

/// @description 一个 controlq command 在完整 display transaction 中的确定性阶段。
#[derive(Clone, Copy)]
pub(super) enum RuntimeStage {
    DisplayInfo,
    UnrefEvicted,
    Create,
    Attach,
    TransferScanout,
    SetScanout,
    FlushScanout,
    UnrefBoot,
    FlushDamage,
    UnrefReleased,
    DisableScanout,
    UnrefDisabled(u8),
}

/// @description 唯一在途 display transaction 及其资源生命周期 owner。
pub(super) enum RuntimeOperation {
    Scanout(ResourceTarget),
    Damage(ResourceTarget),
    Release(ResourceRelease),
    RetireBoot {
        boot: ResourceRelease,
        evicted: Option<ResidentResource>,
    },
    Disable(ResourceSnapshot),
}

/// @description controlq 中唯一在途 command 的 descriptor 与 fence 凭据。
pub(super) struct PendingCommand {
    pub(super) head: u16,
    pub(super) operation_fence: u64,
    pub(super) command_fence: u64,
    pub(super) stage: RuntimeStage,
}

/// @description 一个已 CREATE+ATTACH、可在后续 flip/damage 中复用的 host resource。
pub(super) struct ResidentResource {
    id: u32,
    identity: u64,
    backing: Arc<DeviceBacking>,
    mode: DisplayMode,
    synchronized: bool,
}

/// @description 一次 operation 对固定 residency set 中目标 resource 的独占计划。
pub(super) enum ResourceTarget {
    Resident(usize),
    New {
        slot: usize,
        next: ResidentResource,
        evicted: Option<ResidentResource>,
    },
}

/// @description VirtIO-GPU 唯一的两槽 resource residency owner。
pub(super) struct ResourceSet {
    slots: [Option<ResidentResource>; 2],
    active: Option<usize>,
}

/// @description disable transaction 独占持有、可无损回滚的完整 residency snapshot。
pub(super) struct ResourceSnapshot {
    slots: [Option<ResidentResource>; 2],
    active: Option<usize>,
}

/// @description RMFB 从 residency set 摘下、等待 RESOURCE_UNREF completion 的 owner。
pub(super) struct ResourceRelease {
    slot: usize,
    resource: ResidentResource,
}

impl ResourceSet {
    /// @description 构造 boot initialization 尚未发布 resource 的空集合。
    /// @return 两槽均为空且无 active slot 的集合。
    pub(super) const fn empty() -> Self {
        Self {
            slots: [None, None],
            active: None,
        }
    }

    /// @description 以 firmware boot scanout 建立初始 residency set。
    /// @param backing boot scanout 仍被 device 引用的 SG backing。
    /// @param mode boot resource 的固定 XRGB8888 mode。
    /// @return slot 0 active、slot 1 vacant 的两槽集合。
    pub(super) fn with_boot(backing: Arc<DeviceBacking>, mode: DisplayMode) -> Self {
        Self {
            slots: [
                Some(ResidentResource {
                    id: BOOT_RESOURCE_ID,
                    identity: 0,
                    backing,
                    mode,
                    synchronized: false,
                }),
                None,
            ],
            active: Some(0),
        }
    }

    /// @description 为 stable DRM buffer 取得 resident slot 或预留唯一 inactive slot。
    /// @param identity DRM framebuffer 的全局单调 identity。
    /// @param mode framebuffer 的完整 linear mode。
    /// @param backing 从 publication 到 eviction completion 必须保持存活的 SG owner。
    /// @return resident target，或携带待 CREATE resource 与有界 eviction owner 的 new target。
    /// @errors identity 被复用于不同 backing/mode，或两槽状态损坏时返回 Device。
    pub(super) fn prepare(
        &mut self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<ResourceTarget, DisplayError> {
        if let Some(slot) = self.slots.iter().position(|resource| {
            resource
                .as_ref()
                .is_some_and(|resource| resource.identity == identity)
        }) {
            let resource = self.slots[slot].as_ref().ok_or(DisplayError::Device)?;
            if resource.mode != mode || !Arc::ptr_eq(&resource.backing, &backing) {
                return Err(DisplayError::Device);
            }
            return Ok(ResourceTarget::Resident(slot));
        }

        let slot = self
            .slots
            .iter()
            .position(Option::is_none)
            .or_else(|| {
                self.slots.iter().enumerate().find_map(|(slot, resource)| {
                    (resource.is_some() && self.active != Some(slot)).then_some(slot)
                })
            })
            .ok_or(DisplayError::Device)?;
        let evicted = self.slots[slot].take();
        Ok(ResourceTarget::New {
            slot,
            next: ResidentResource {
                id: RESOURCE_IDS[slot],
                identity,
                backing,
                mode,
                synchronized: false,
            },
            evicted,
        })
    }

    /// @description 原子提交 target 的 residency/synchronization 结果。
    /// @param target 当前唯一 operation 持有的 target。
    /// @param activate 完成后该 slot 是否成为 hardware scanout。
    /// @param synchronized userspace 显式 DIRTYFB 后是否可跳过下一次 full transfer。
    /// @return 已完成 RESOURCE_UNREF、可在 control lock 外析构的旧 resource。
    pub(super) fn complete(
        &mut self,
        target: ResourceTarget,
        activate: bool,
        synchronized: bool,
    ) -> Option<ResidentResource> {
        let (slot, evicted) = match target {
            ResourceTarget::Resident(slot) => (slot, None),
            ResourceTarget::New {
                slot,
                next,
                evicted,
            } => {
                assert!(
                    self.slots[slot].is_none(),
                    "GPU residency slot was republished"
                );
                self.slots[slot] = Some(next);
                (slot, evicted)
            }
        };
        self.slots[slot]
            .as_mut()
            .expect("completed GPU target is not resident")
            .synchronized = synchronized;
        if activate {
            self.active = Some(slot);
        }
        evicted
    }

    /// @description 回滚尚未进入 avail ring 的 target reservation。
    /// @param target publication 前失败的独占 target。
    /// @return 未发布的新 resource，供 caller 在 control lock 外析构。
    pub(super) fn cancel(&mut self, target: ResourceTarget) -> Option<ResidentResource> {
        match target {
            ResourceTarget::Resident(_) => None,
            ResourceTarget::New {
                slot,
                next,
                evicted,
            } => {
                assert!(self.slots[slot].is_none(), "cancelled GPU slot is occupied");
                self.slots[slot] = evicted;
                Some(next)
            }
        }
    }

    /// @description 读取 active resource mode，供 resource_id=0 disable transaction 使用。
    /// @return 存在 active resource 时返回其 mode。
    pub(super) fn active_mode(&self) -> Option<DisplayMode> {
        self.active
            .and_then(|slot| self.slots[slot].as_ref().map(|resource| resource.mode))
    }

    /// @description 摘下一个 inactive framebuffer 的 resident resource。
    /// @param identity DRM framebuffer 的全局单调 identity。
    /// @return 未 resident 时为 None；resident 时返回独占 release owner。
    /// @errors identity 仍是 active scanout 时返回 Device，必须先走 disable transaction。
    pub(super) fn release(
        &mut self,
        identity: u64,
    ) -> Result<Option<ResourceRelease>, DisplayError> {
        let Some(slot) = self.slots.iter().position(|resource| {
            resource
                .as_ref()
                .is_some_and(|resource| resource.identity == identity)
        }) else {
            return Ok(None);
        };
        if self.active == Some(slot) {
            return Err(DisplayError::Device);
        }
        Ok(Some(ResourceRelease {
            slot,
            resource: self.slots[slot]
                .take()
                .expect("located GPU resource disappeared"),
        }))
    }

    /// @description 恢复尚未进入 avail ring 的 RMFB release reservation。
    /// @param release publication 前失败的完整 resource owner。
    pub(super) fn restore_release(&mut self, release: ResourceRelease) {
        assert!(self.slots[release.slot].is_none());
        self.slots[release.slot] = Some(release.resource);
    }

    /// @description 把全部 residency owner 移交给 disable operation。
    /// @return 最多两个仍需 RESOURCE_UNREF 的 resource。
    pub(super) fn take_all(&mut self) -> ResourceSnapshot {
        ResourceSnapshot {
            slots: core::mem::take(&mut self.slots),
            active: self.active.take(),
        }
    }

    /// @description 恢复尚未发布的 disable transaction。
    /// @param resources take_all 返回且尚未进入 avail ring 的完整集合。
    pub(super) fn restore_all(&mut self, resources: ResourceSnapshot) {
        assert!(self.slots.iter().all(Option::is_none));
        self.active = resources.active;
        self.slots = resources.slots;
    }

    fn resident(&self, slot: usize) -> &ResidentResource {
        self.slots[slot]
            .as_ref()
            .expect("resident GPU target lost its slot")
    }
}

impl ResourceTarget {
    /// @description 判断 target 是否需要 CREATE+ATTACH。
    /// @return 尚未 resident 时返回 true。
    pub(super) fn is_new(&self) -> bool {
        matches!(self, Self::New { .. })
    }

    /// @description 返回必须先完成 UNREF 的 bounded eviction resource ID。
    /// @return 占用目标 inactive slot 的旧 resource ID；无 eviction 时返回 None。
    pub(super) fn evicted_id(&self) -> Option<u32> {
        match self {
            Self::Resident(_) => None,
            Self::New { evicted, .. } => evicted.as_ref().map(|resource| resource.id),
        }
    }

    /// @description 返回 target 对应的 stable VirtIO resource ID。
    /// @param resources resident target 的唯一 residency owner。
    /// @return target 绑定的固定两槽 resource ID。
    pub(super) fn id(&self, resources: &ResourceSet) -> u32 {
        match self {
            Self::Resident(slot) => resources.resident(*slot).id,
            Self::New { next, .. } => next.id,
        }
    }

    /// @description 返回 target 捕获的 framebuffer mode。
    /// @param resources resident target 的唯一 residency owner。
    /// @return target 创建或复用时验证过的 canonical mode。
    pub(super) fn mode(&self, resources: &ResourceSet) -> DisplayMode {
        match self {
            Self::Resident(slot) => resources.resident(*slot).mode,
            Self::New { next, .. } => next.mode,
        }
    }

    /// @description 克隆 target 的 SG lifetime owner，供 request codec 锁内短借用。
    /// @param resources resident target 的唯一 residency owner。
    /// @return 保证 command completion 前 backing 存活的共享 owner。
    pub(super) fn backing_owner(&self, resources: &ResourceSet) -> Arc<DeviceBacking> {
        match self {
            Self::Resident(slot) => resources.resident(*slot).backing.clone(),
            Self::New { next, .. } => next.backing.clone(),
        }
    }

    /// @description 判断 resident target 是否已由显式 DIRTYFB 同步到 host。
    /// @param resources resident target 的唯一 residency owner。
    /// @return resident 且最近一次 DIRTYFB 已完成时返回 true；new target 返回 false。
    pub(super) fn synchronized(&self, resources: &ResourceSet) -> bool {
        match self {
            Self::Resident(slot) => resources.resident(*slot).synchronized,
            Self::New { .. } => false,
        }
    }
}

impl ResidentResource {
    /// @description 返回 disable operation 要解绑的 VirtIO resource ID。
    /// @return resource 创建时绑定的固定两槽 ID。
    pub(super) fn id(&self) -> u32 {
        self.id
    }
}

impl ResourceRelease {
    /// @description 返回 RMFB transaction 要解绑的 VirtIO resource ID。
    /// @return release 独占持有的 resident resource ID。
    pub(super) fn id(&self) -> u32 {
        self.resource.id
    }
}

/// @description 构造覆盖整个 framebuffer 的 canonical damage rectangle。
/// @param mode framebuffer 的有效 mode。
/// @return 原点为零、尺寸等于 mode 的 rectangle。
pub(super) fn full_rectangle(mode: DisplayMode) -> crate::drivers::DisplayRect {
    crate::drivers::DisplayRect {
        x: 0,
        y: 0,
        width: mode.width,
        height: mode.height,
    }
}

/// @description 从 scanout/damage transaction 取得其唯一 resource target。
/// @param operation 当前唯一在途 display transaction。
/// @return scanout 或 damage 持有的 target 借用。
/// @errors operation 缺失或类型不拥有 target 时返回 Device。
pub(super) fn operation_target_ref(
    operation: &Option<RuntimeOperation>,
) -> Result<&ResourceTarget, DisplayError> {
    match operation.as_ref() {
        Some(RuntimeOperation::Scanout(target) | RuntimeOperation::Damage(target)) => Ok(target),
        _ => Err(DisplayError::Device),
    }
}

/// @description 解析当前 transaction target 的 mode 与 stable VirtIO resource ID。
/// @param operation 当前唯一在途 display transaction。
/// @param resources resident target 的唯一 residency owner。
/// @return target 的 canonical mode 与 VirtIO resource ID。
/// @errors operation 缺失或类型不拥有 target 时返回 Device。
pub(super) fn operation_target(
    operation: &Option<RuntimeOperation>,
    resources: &ResourceSet,
) -> Result<(DisplayMode, u32), DisplayError> {
    let target = operation_target_ref(operation)?;
    Ok((target.mode(resources), target.id(resources)))
}

/// @description 从 disable snapshot 中查找指定 slot 起的下一只 resource。
/// @param operation 必须是持有完整 snapshot 的 disable transaction。
/// @param start 首个允许返回的 slot index。
/// @return 下一只 resource 的 slot 与 VirtIO ID；不存在时返回 None。
/// @errors operation 不是 disable transaction 时返回 Device。
pub(super) fn disabled_resource(
    operation: &Option<RuntimeOperation>,
    start: usize,
) -> Result<Option<(usize, u32)>, DisplayError> {
    let resources = match operation.as_ref() {
        Some(RuntimeOperation::Disable(resources)) => resources,
        _ => return Err(DisplayError::Device),
    };
    Ok(resources
        .slots
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(slot, resource)| resource.as_ref().map(|resource| (slot, resource.id()))))
}

impl VirtIOGpuDevice {
    /// @description 以两槽 residency protocol 提交 scanout switch。
    /// @param identity DRM framebuffer 的全局单调 identity。
    /// @param mode target framebuffer 的 canonical mode。
    /// @param backing target SG lifetime owner。
    /// @return 完整 switch operation fence。
    /// @errors backing/mode、residency 或 controlq publication failure。
    pub(super) fn submit_resident_scanout(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError> {
        validate_backing(mode, &backing)?;
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        if control.mode != mode {
            return Err(DisplayError::InvalidRectangle);
        }
        let target = control.resources.prepare(identity, mode, backing)?;
        let resource_id = target.id(&control.resources);
        let prepared = (|| {
            if let Some(evicted) = target.evicted_id() {
                prepare_unref(&mut control.request, evicted)?;
                Ok((
                    super::VIRTIO_GPU_CMD_RESOURCE_UNREF,
                    32,
                    RuntimeStage::UnrefEvicted,
                ))
            } else if target.is_new() {
                super::prepare_create(&mut control.request, mode, resource_id)?;
                Ok((
                    super::VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                    40,
                    RuntimeStage::Create,
                ))
            } else if target.synchronized(&control.resources) {
                super::prepare_set_scanout(&mut control.request, mode, resource_id)?;
                Ok((
                    super::VIRTIO_GPU_CMD_SET_SCANOUT,
                    48,
                    RuntimeStage::SetScanout,
                ))
            } else {
                super::prepare_transfer(
                    &mut control.request,
                    mode,
                    full_rectangle(mode),
                    resource_id,
                )?;
                Ok((
                    super::VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                    56,
                    RuntimeStage::TransferScanout,
                ))
            }
        })();
        let (command, length, stage) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let unpublished = control.resources.cancel(target);
                drop(control);
                drop(unpublished);
                return Err(error);
            }
        };
        control.operation = Some(RuntimeOperation::Scanout(target));
        let result = self.publish_runtime(&mut control, command, length, None, stage);
        if result.is_err() {
            let target = match control.operation.take() {
                Some(RuntimeOperation::Scanout(target)) => target,
                _ => unreachable!(),
            };
            let unpublished = control.resources.cancel(target);
            drop(control);
            drop(unpublished);
        }
        result
    }

    /// @description 提交 RMFB 对 inactive resident resource 的显式 UNREF。
    /// @param identity DRM framebuffer 的全局单调 identity。
    /// @return 未 resident 时为 None；否则返回完整 release operation fence。
    /// @errors active identity、已有 operation 或 controlq publication failure。
    pub(super) fn release_resident(&self, identity: u64) -> Result<Option<u64>, DisplayError> {
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let Some(release) = control.resources.release(identity)? else {
            return Ok(None);
        };
        if let Err(error) = prepare_unref(&mut control.request, release.id()) {
            control.resources.restore_release(release);
            return Err(error);
        }
        control.operation = Some(RuntimeOperation::Release(release));
        let result = self.publish_runtime(
            &mut control,
            VIRTIO_GPU_CMD_RESOURCE_UNREF,
            32,
            None,
            RuntimeStage::UnrefReleased,
        );
        if result.is_err() {
            let release = match control.operation.take() {
                Some(RuntimeOperation::Release(release)) => release,
                _ => unreachable!(),
            };
            control.resources.restore_release(release);
        }
        result.map(Some)
    }

    /// @description 以 resource_id=0 禁用 scanout 并移交全部 residency owner。
    /// @return SET_SCANOUT→UNREF transaction fence。
    /// @errors 无 active resource、已有 operation 或 publication failure。
    pub(super) fn disable_resident(&self) -> Result<u64, DisplayError> {
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let mode = control
            .resources
            .active_mode()
            .ok_or(DisplayError::Device)?;
        super::prepare_set_scanout(&mut control.request, mode, 0)?;
        let resources = control.resources.take_all();
        control.operation = Some(RuntimeOperation::Disable(resources));
        let result = self.publish_runtime(
            &mut control,
            super::VIRTIO_GPU_CMD_SET_SCANOUT,
            48,
            None,
            RuntimeStage::DisableScanout,
        );
        if result.is_err() {
            let resources = match control.operation.take() {
                Some(RuntimeOperation::Disable(resources)) => resources,
                _ => unreachable!(),
            };
            control.resources.restore_all(resources);
        }
        result
    }
}
