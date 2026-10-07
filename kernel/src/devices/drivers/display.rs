use alloc::sync::Arc;

use crate::memory::DeviceBacking;

/// single-scanout adapter 的 canonical CVT/scanout 显示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayMode {
    /// 8-pixel granular 水平 pixel 数；connector、resource 与 scanout 必须共用该值。
    pub(crate) width: u32,
    /// 垂直 pixel 数。
    pub(crate) height: u32,
    /// XRGB8888 每行字节数。
    pub(crate) pitch: u32,
}

/// scanout 坐标系中的半开 damage rectangle。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DisplayRect {
    /// 左上角水平 pixel。
    pub(crate) x: u32,
    /// 左上角垂直 pixel。
    pub(crate) y: u32,
    /// 非零水平 pixel 数。
    pub(crate) width: u32,
    /// 非零垂直 pixel 数。
    pub(crate) height: u32,
}

/// display command 的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayError {
    /// 已有 command 尚未完成，调用方应等待 completion edge。
    WouldBlock,
    /// rectangle 越过当前 scanout。
    InvalidRectangle,
    /// transport、queue 或 response 损坏。
    Device,
}

/// deferred display work 对上层发布的单一更新事实。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayUpdate {
    /// 一次 userspace scanout、damage 或 disable operation 已完整完成。
    OperationCompleted(u64),
    /// adapter 重新查询 display-info 后观察到新的 scanout mode。
    ModeChanged(DisplayMode),
    /// adapter 内部 display-info 查询完成且 preferred mode 未变化。
    ///
    /// DRM 同步 ioctl 可能正在等待 controlq 从该内部事务重新变为可提交；
    /// 缺失这个 edge 会让一次无实际 mode 变化的 config interrupt 永久阻塞 waiter。
    AdapterReady,
    /// 一条 VirGL controlq operation 已完成，fence 可由 DRM waiter 观察。
    RenderCompleted(u64),
    /// 一条独立 cursorq operation 已完成，不与 controlq fence 共用命名空间。
    CursorCompleted(u64),
}

/// 不泄漏具体 adapter 的 single-scanout display seam。
pub(crate) trait DisplayDevice: Send + Sync {
    /// 绑定消费者分配的 completion deferred vector；绑定前到达的 completion 由绑定时的一次
    /// 发布补偿。
    fn bind_completion_work(&self, work: crate::deferred::DeferredWork);

    /// 返回 connector 最新 preferred mode。
    ///
    /// # Returns
    ///
    /// 同一代 width、height 与 pitch；与 active CRTC mode 相互独立。
    fn mode(&self) -> DisplayMode;

    /// 异步把一个 XRGB8888 scatter/gather backing 切换为指定 scanout mode。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity，用于命中 resident resource。
    /// - `mode`: 本次 transaction 捕获的 display-info mode。
    /// - `backing`: 至少覆盖固定 mode pitch × height；adapter 从提交到资源解绑完成独立
    ///   保活该 owner。
    ///
    /// # Returns
    ///
    /// operation fence；已有 transaction 时返回 `WouldBlock`。
    ///
    /// # Errors
    ///
    /// backing 太小返回 `InvalidRectangle`；queue 满、MMIO 或 response 失败返回
    /// `Device`。
    fn submit_scanout(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError>;

    /// 把指定 stable framebuffer 的若干 damage rectangle 批量传输到 host。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity，用于复用 resident resource。
    /// - `mode`: target framebuffer 的完整 linear mode。
    /// - `backing`: target framebuffer 的 SG owner；adapter 保活到 eviction completion。
    /// - `rectangles`: 1..=32 个已合并、非空且位于 target mode 内的 rectangle。
    ///
    /// # Returns
    ///
    /// blocking DIRTYFB 等待的 operation fence。
    ///
    /// # Errors
    ///
    /// identity/backing 不一致、rectangle 越界、已有 operation 或 device failure。
    fn submit_damage(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
        rectangles: &[DisplayRect],
    ) -> Result<u64, DisplayError>;

    /// 释放一个已删除 framebuffer 对应的 inactive resident resource。
    ///
    /// # Parameters
    ///
    /// - `identity`: DRM framebuffer 的全局单调 identity。
    ///
    /// # Returns
    ///
    /// resource 未 resident 时为 None；否则返回 RESOURCE_UNREF operation fence。
    ///
    /// # Errors
    ///
    /// identity 仍 active、已有 operation 或 device failure。
    fn release_buffer(&self, identity: u64) -> Result<Option<u64>, DisplayError>;

    /// 以标准 resource_id=0 禁用 scanout，再解绑并释放 active resource。
    ///
    /// # Returns
    ///
    /// disable operation fence；hardware 不再引用 backing 后才完成。
    ///
    /// # Errors
    ///
    /// 无 active resource、已有 operation 或 device failure。
    fn disable_scanout(&self) -> Result<u64, DisplayError>;

    /// 有界消费一个 controlq/config 更新，并推进 transaction state。
    ///
    /// # Returns
    ///
    /// scanout 最终完成或 mode 改变时返回领域更新；无更新返回 `None`。
    ///
    /// # Errors
    ///
    /// descriptor、fence 或 device response 不匹配返回 `Device`。
    fn poll_update(&self) -> Result<Option<DisplayUpdate>, DisplayError>;
}
