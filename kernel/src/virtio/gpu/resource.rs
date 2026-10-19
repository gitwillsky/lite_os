use alloc::sync::Arc;

use crate::{
    drivers::{DisplayError, DisplayMode},
    memory::{DeviceBacking, PAGE_SIZE},
};

use super::{
    VirtIOGpuDevice,
    command::{GpuCommand, ScanoutPurpose, UnrefPurpose},
    wire::{ALTERNATE_RESOURCE_ID, BOOT_RESOURCE_ID, CURSOR_RESOURCE_ID},
};

const RESOURCE_IDS: [u32; 2] = [BOOT_RESOURCE_ID, ALTERNATE_RESOURCE_ID];

/// 验证 framebuffer mode 可由给定 SG backing 完整覆盖。
///
/// # Parameters
///
/// - `mode`: framebuffer 的 canonical linear mode。
/// - `backing`: 在 device operation 完成前保持存活的 SG owner。
///
/// # Returns
///
/// backing 容量和 VirtIO 32-bit length 均合法时返回成功。
///
/// # Errors
///
/// pitch×height 溢出、超过 backing 或超过 VirtIO length 时返回 InvalidRectangle。
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

/// 唯一在途 display transaction 及其资源生命周期 owner。
pub(super) enum RuntimeOperation {
    Scanout(ResourceTarget),
    Damage(ResourceTarget),
    CursorUpload(CursorTarget),
    Release(ResourceRelease),
    RetireBoot {
        boot: ResourceRelease,
        evicted: Option<ResidentResource>,
    },
    Disable(ResourceSnapshot),
}

/// 已 CREATE+ATTACH、由 cursorq 唯一读取的标准 2D cursor resource。
pub(super) struct CursorResource {
    identity: u64,
    backing: Arc<DeviceBacking>,
}

/// 一次 cursor upload 对固定 resource ID 的独占替换计划。
pub(super) enum CursorTarget {
    Resident,
    New {
        next: CursorResource,
        evicted: Option<CursorResource>,
    },
}

/// VirtIO-GPU 唯一 64x64 ARGB 2D cursor resource owner。
pub(super) struct CursorResourceSet {
    resident: Option<CursorResource>,
}

impl CursorResourceSet {
    /// 构造尚未发布 cursor resource 的初始状态。
    pub(super) const fn empty() -> Self {
        Self { resident: None }
    }

    /// 为一个 stable dumb buffer 准备复用或替换固定 cursor resource。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM dumb buffer 的全局单调 identity。
    /// - `backing`: 64x64 ARGB pixels 的 SG lifetime owner。
    ///
    /// # Returns
    ///
    /// 当前 resource target；不同 identity 会先独占摘下旧 owner。
    ///
    /// # Errors
    ///
    /// identity 被复用于不同 backing 时返回 Device。
    pub(super) fn prepare(
        &mut self,
        identity: u64,
        backing: Arc<DeviceBacking>,
    ) -> Result<CursorTarget, DisplayError> {
        if let Some(resident) = self.resident.as_ref()
            && resident.identity == identity
        {
            if !Arc::ptr_eq(&resident.backing, &backing) {
                return Err(DisplayError::Device);
            }
            return Ok(CursorTarget::Resident);
        }
        Ok(CursorTarget::New {
            next: CursorResource { identity, backing },
            evicted: self.resident.take(),
        })
    }

    /// 发布已完成 CREATE+ATTACH+TRANSFER 的 cursor target。
    ///
    /// # Returns
    ///
    /// 被替换且已完成 RESOURCE_UNREF 的旧 backing owner。
    pub(super) fn complete(&mut self, target: CursorTarget) -> Option<CursorResource> {
        match target {
            CursorTarget::Resident => None,
            CursorTarget::New { next, evicted } => {
                assert!(self.resident.is_none(), "cursor resource was republished");
                self.resident = Some(next);
                evicted
            }
        }
    }

    /// 回滚尚未进入 avail ring 的 cursor replacement。
    ///
    /// # Returns
    ///
    /// 未发布的新 backing owner。
    pub(super) fn cancel(&mut self, target: CursorTarget) -> Option<CursorResource> {
        match target {
            CursorTarget::Resident => None,
            CursorTarget::New { next, evicted } => {
                assert!(
                    self.resident.is_none(),
                    "cancelled cursor resource is occupied"
                );
                self.resident = evicted;
                Some(next)
            }
        }
    }
}

impl CursorTarget {
    /// 返回 target 使用的固定 VirtIO cursor resource ID。
    pub(super) const fn id(&self) -> u32 {
        CURSOR_RESOURCE_ID
    }

    /// 返回替换前是否必须先完成 RESOURCE_UNREF。
    pub(super) fn evicts(&self) -> bool {
        matches!(
            self,
            Self::New {
                evicted: Some(_),
                ..
            }
        )
    }

    /// 返回是否必须执行 CREATE+ATTACH，而非仅上传已有 resource。
    pub(super) fn is_new(&self) -> bool {
        matches!(self, Self::New { .. })
    }

    /// 克隆本次 upload 必须保活的 SG backing owner。
    pub(super) fn backing_owner(&self, resources: &CursorResourceSet) -> Arc<DeviceBacking> {
        match self {
            Self::Resident => resources
                .resident
                .as_ref()
                .expect("resident cursor target disappeared")
                .backing
                .clone(),
            Self::New { next, .. } => next.backing.clone(),
        }
    }
}

/// 一个已 CREATE+ATTACH、可在后续 flip/damage 中复用的 host resource。
pub(super) struct ResidentResource {
    id: u32,
    identity: u64,
    backing: Arc<DeviceBacking>,
    mode: DisplayMode,
    synchronized: bool,
}

/// 一次 operation 对固定 residency set 中目标 resource 的独占计划。
pub(super) enum ResourceTarget {
    Resident(usize),
    New {
        slot: usize,
        next: ResidentResource,
        evicted: Option<ResidentResource>,
    },
}

/// VirtIO-GPU 唯一的两槽 resource residency owner。
pub(super) struct ResourceSet {
    slots: [Option<ResidentResource>; 2],
    active: Option<usize>,
}

/// disable transaction 独占持有、可无损回滚的完整 residency snapshot。
pub(super) struct ResourceSnapshot {
    slots: [Option<ResidentResource>; 2],
    active: Option<usize>,
}

/// RMFB 从 residency set 摘下、等待 RESOURCE_UNREF completion 的 owner。
pub(super) struct ResourceRelease {
    slot: usize,
    resource: ResidentResource,
}

impl ResourceSet {
    /// 构造 boot initialization 尚未发布 resource 的空集合。
    ///
    /// # Returns
    ///
    /// 两槽均为空且无 active slot 的集合。
    pub(super) const fn empty() -> Self {
        Self {
            slots: [None, None],
            active: None,
        }
    }

    /// 以 firmware boot scanout 建立初始 residency set。
    ///
    /// # Parameters
    ///
    /// - `backing`: boot scanout 仍被 device 引用的 SG backing。
    /// - `mode`: boot resource 的固定 XRGB8888 mode。
    ///
    /// # Returns
    ///
    /// slot 0 active、slot 1 vacant 的两槽集合。
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

    /// 为 stable DRM buffer 取得 resident slot 或预留唯一 inactive slot。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity。
    /// - `mode`: framebuffer 的完整 linear mode。
    /// - `backing`: 从 publication 到 eviction completion 必须保持存活的 SG owner。
    ///
    /// # Returns
    ///
    /// resident target，或携带待 CREATE resource 与有界 eviction owner 的 new target。
    ///
    /// # Errors
    ///
    /// identity 被复用于不同 backing/mode，或两槽状态损坏时返回 Device。
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

    /// 原子提交 target 的 residency/synchronization 结果。
    ///
    /// # Parameters
    ///
    /// - `target`: 当前唯一 operation 持有的 target。
    /// - `activate`: 完成后该 slot 是否成为 hardware scanout。
    /// - `synchronized`: userspace 显式 DIRTYFB 后是否可跳过下一次 full transfer。
    ///
    /// # Returns
    ///
    /// 已完成 RESOURCE_UNREF、可在 control lock 外析构的旧 resource。
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

    /// 在 VirGL framebuffer 接管 hardware scanout 后撤销 2D active slot。
    ///
    /// # Returns
    ///
    /// 无返回值；resident resource 仍保留到统一 disable/RMFB owner 回收。
    pub(super) fn deactivate(&mut self) {
        self.active = None;
    }

    /// 回滚尚未进入 avail ring 的 target reservation。
    ///
    /// # Parameters
    ///
    /// - `target`: publication 前失败的独占 target。
    ///
    /// # Returns
    ///
    /// 未发布的新 resource，供 caller 在 control lock 外析构。
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

    /// 摘下一个 inactive framebuffer 的 resident resource。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity。
    ///
    /// # Returns
    ///
    /// 未 resident 时为 None；resident 时返回独占 release owner。
    ///
    /// # Errors
    ///
    /// identity 仍是 active scanout 时返回 Device，必须先走 disable transaction。
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

    /// 恢复尚未进入 avail ring 的 RMFB release reservation。
    ///
    /// # Parameters
    ///
    /// - `release`: publication 前失败的完整 resource owner。
    pub(super) fn restore_release(&mut self, release: ResourceRelease) {
        assert!(self.slots[release.slot].is_none());
        self.slots[release.slot] = Some(release.resource);
    }

    /// 把全部 residency owner 移交给 disable operation。
    ///
    /// # Returns
    ///
    /// 最多两个仍需 RESOURCE_UNREF 的 resource。
    pub(super) fn take_all(&mut self) -> ResourceSnapshot {
        ResourceSnapshot {
            slots: core::mem::take(&mut self.slots),
            active: self.active.take(),
        }
    }

    /// 恢复尚未发布的 disable transaction。
    ///
    /// # Parameters
    ///
    /// - `resources`: take_all 返回且尚未进入 avail ring 的完整集合。
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
    /// 判断 target 是否需要 CREATE+ATTACH。
    ///
    /// # Returns
    ///
    /// 尚未 resident 时返回 true。
    pub(super) fn is_new(&self) -> bool {
        matches!(self, Self::New { .. })
    }

    /// 返回必须先完成 UNREF 的 bounded eviction resource ID。
    ///
    /// # Returns
    ///
    /// 占用目标 inactive slot 的旧 resource ID；无 eviction 时返回 None。
    pub(super) fn evicted_id(&self) -> Option<u32> {
        match self {
            Self::Resident(_) => None,
            Self::New { evicted, .. } => evicted.as_ref().map(|resource| resource.id),
        }
    }

    /// 返回 target 对应的 stable VirtIO resource ID。
    ///
    /// # Parameters
    ///
    /// - `resources`: resident target 的唯一 residency owner。
    ///
    /// # Returns
    ///
    /// target 绑定的固定两槽 resource ID。
    pub(super) fn id(&self, resources: &ResourceSet) -> u32 {
        match self {
            Self::Resident(slot) => resources.resident(*slot).id,
            Self::New { next, .. } => next.id,
        }
    }

    /// 返回 target 捕获的 framebuffer mode。
    ///
    /// # Parameters
    ///
    /// - `resources`: resident target 的唯一 residency owner。
    ///
    /// # Returns
    ///
    /// target 创建或复用时验证过的 canonical mode。
    pub(super) fn mode(&self, resources: &ResourceSet) -> DisplayMode {
        match self {
            Self::Resident(slot) => resources.resident(*slot).mode,
            Self::New { next, .. } => next.mode,
        }
    }

    /// 克隆 target 的 SG lifetime owner，供 request codec 锁内短借用。
    ///
    /// # Parameters
    ///
    /// - `resources`: resident target 的唯一 residency owner。
    ///
    /// # Returns
    ///
    /// 保证 command completion 前 backing 存活的共享 owner。
    pub(super) fn backing_owner(&self, resources: &ResourceSet) -> Arc<DeviceBacking> {
        match self {
            Self::Resident(slot) => resources.resident(*slot).backing.clone(),
            Self::New { next, .. } => next.backing.clone(),
        }
    }

    /// 判断 resident target 是否已由显式 DIRTYFB 同步到 host。
    ///
    /// # Parameters
    ///
    /// - `resources`: resident target 的唯一 residency owner。
    ///
    /// # Returns
    ///
    /// resident 且最近一次 DIRTYFB 已完成时返回 true；new target 返回 false。
    pub(super) fn synchronized(&self, resources: &ResourceSet) -> bool {
        match self {
            Self::Resident(slot) => resources.resident(*slot).synchronized,
            Self::New { .. } => false,
        }
    }
}

impl ResidentResource {
    /// 返回 disable operation 要解绑的 VirtIO resource ID。
    ///
    /// # Returns
    ///
    /// resource 创建时绑定的固定两槽 ID。
    pub(super) fn id(&self) -> u32 {
        self.id
    }
}

impl ResourceRelease {
    /// 返回 RMFB transaction 要解绑的 VirtIO resource ID。
    ///
    /// # Returns
    ///
    /// release 独占持有的 resident resource ID。
    pub(super) fn id(&self) -> u32 {
        self.resource.id
    }
}

/// 构造覆盖整个 framebuffer 的 canonical damage rectangle。
///
/// # Parameters
///
/// - `mode`: framebuffer 的有效 mode。
///
/// # Returns
///
/// 原点为零、尺寸等于 mode 的 rectangle。
pub(super) fn full_rectangle(mode: DisplayMode) -> crate::drivers::DisplayRect {
    crate::drivers::DisplayRect {
        x: 0,
        y: 0,
        width: mode.width,
        height: mode.height,
    }
}

/// 从 scanout/damage transaction 取得其唯一 resource target。
///
/// # Parameters
///
/// - `operation`: 当前唯一在途 display transaction。
///
/// # Returns
///
/// scanout 或 damage 持有的 target 借用。
///
/// # Errors
///
/// operation 缺失或类型不拥有 target 时返回 Device。
pub(super) fn operation_target_ref(
    operation: &Option<RuntimeOperation>,
) -> Result<&ResourceTarget, DisplayError> {
    match operation.as_ref() {
        Some(RuntimeOperation::Scanout(target) | RuntimeOperation::Damage(target)) => Ok(target),
        _ => Err(DisplayError::Device),
    }
}

/// 解析当前 transaction target 的 mode 与 stable VirtIO resource ID。
///
/// # Parameters
///
/// - `operation`: 当前唯一在途 display transaction。
/// - `resources`: resident target 的唯一 residency owner。
///
/// # Returns
///
/// target 的 canonical mode 与 VirtIO resource ID。
///
/// # Errors
///
/// operation 缺失或类型不拥有 target 时返回 Device。
pub(super) fn operation_target(
    operation: &Option<RuntimeOperation>,
    resources: &ResourceSet,
) -> Result<(DisplayMode, u32), DisplayError> {
    let target = operation_target_ref(operation)?;
    Ok((target.mode(resources), target.id(resources)))
}

/// 从 disable snapshot 中查找指定 slot 起的下一只 resource。
///
/// # Parameters
///
/// - `operation`: 必须是持有完整 snapshot 的 disable transaction。
/// - `start`: 首个允许返回的 slot index。
///
/// # Returns
///
/// 下一只 resource 的 slot 与 VirtIO ID；不存在时返回 None。
///
/// # Errors
///
/// operation 不是 disable transaction 时返回 Device。
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
    /// 上传一个标准 DRM dumb buffer 到唯一 VirtIO 2D cursor resource。
    ///
    /// # Parameters
    ///
    /// - `identity`: dumb buffer 的 stable device identity。
    /// - `backing`: 64x64x4 cursor backing；operation completion 前保持存活。
    ///
    /// # Returns
    ///
    /// 替换旧 resource 时完成 UNREF→CREATE→ATTACH→TRANSFER，否则完成 TRANSFER
    /// 的单一 fence。
    ///
    /// # Errors
    ///
    /// backing geometry、control transaction 或 queue publication failure。
    pub(super) fn submit_cursor_resource_upload(
        &self,
        identity: u64,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError> {
        let mode = DisplayMode {
            width: 64,
            height: 64,
            pitch: 64 * 4,
        };
        validate_backing(mode, &backing)?;
        let mut control = self.control.lock();
        if control.operation.is_some()
            || control.commands.has_non_render_pending()
            || control.damage.batch_active()
        {
            return Err(DisplayError::WouldBlock);
        }
        let target = control.cursor_resource.prepare(identity, backing)?;
        let resource_id = target.id();
        let command = if target.evicts() {
            GpuCommand::Unref {
                resource_id,
                purpose: UnrefPurpose::Cursor,
            }
        } else if target.is_new() {
            GpuCommand::CreateCursor { resource_id }
        } else {
            GpuCommand::TransferCursor { resource_id }
        };
        control.operation = Some(RuntimeOperation::CursorUpload(target));
        let result = self.submit_command(&mut control, command, None);
        if result.is_err() {
            let target = match control.operation.take() {
                Some(RuntimeOperation::CursorUpload(target)) => target,
                _ => unreachable!(),
            };
            let unpublished = control.cursor_resource.cancel(target);
            drop(control);
            drop(unpublished);
        }
        result
    }

    /// 以两槽 residency protocol 提交 scanout switch。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity。
    /// - `mode`: target framebuffer 的 canonical mode。
    /// - `backing`: target SG lifetime owner。
    ///
    /// # Returns
    ///
    /// 完整 switch operation fence。
    ///
    /// # Errors
    ///
    /// backing/mode、residency 或 controlq publication failure。
    pub(super) fn submit_resident_scanout(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError> {
        validate_backing(mode, &backing)?;
        let mut control = self.control.lock();
        if control.commands.has_pending() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let target = control.resources.prepare(identity, mode, backing)?;
        let resource_id = target.id(&control.resources);
        let command = if let Some(evicted) = target.evicted_id() {
            GpuCommand::Unref {
                resource_id: evicted,
                purpose: UnrefPurpose::Evicted,
            }
        } else if target.is_new() {
            GpuCommand::Create { mode, resource_id }
        } else if target.synchronized(&control.resources) {
            GpuCommand::SetScanout {
                mode,
                resource_id,
                purpose: ScanoutPurpose::Activate,
            }
        } else {
            GpuCommand::TransferScanout { mode, resource_id }
        };
        control.operation = Some(RuntimeOperation::Scanout(target));
        let result = self.submit_command(&mut control, command, None);
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

    /// 提交 RMFB 对 inactive resident resource 的显式 UNREF。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity。
    ///
    /// # Returns
    ///
    /// 未 resident 时为 None；否则返回完整 release operation fence。
    ///
    /// # Errors
    ///
    /// active identity、已有 operation 或 controlq publication failure。
    pub(super) fn release_resident(&self, identity: u64) -> Result<Option<u64>, DisplayError> {
        let mut control = self.control.lock();
        if control.commands.has_pending() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let Some(release) = control.resources.release(identity)? else {
            return Ok(None);
        };
        let command = GpuCommand::Unref {
            resource_id: release.id(),
            purpose: UnrefPurpose::Released,
        };
        control.operation = Some(RuntimeOperation::Release(release));
        let result = self.submit_command(&mut control, command, None);
        if result.is_err() {
            let release = match control.operation.take() {
                Some(RuntimeOperation::Release(release)) => release,
                _ => unreachable!(),
            };
            control.resources.restore_release(release);
        }
        result.map(Some)
    }

    /// 以 resource_id=0 禁用 scanout 并移交全部 residency owner。
    ///
    /// # Returns
    ///
    /// SET_SCANOUT→UNREF transaction fence。
    ///
    /// # Errors
    ///
    /// 无 completion-confirmed scanout、已有 operation 或 controlq publication failure。
    pub(super) fn disable_active_scanout(&self) -> Result<u64, DisplayError> {
        let mut control = self.control.lock();
        if control.commands.has_pending() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let mode = control.scanout.mode().ok_or(DisplayError::Device)?;
        let resources = control.resources.take_all();
        control.operation = Some(RuntimeOperation::Disable(resources));
        let result = self.submit_command(
            &mut control,
            GpuCommand::SetScanout {
                mode,
                resource_id: 0,
                purpose: ScanoutPurpose::Disable,
            },
            None,
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
