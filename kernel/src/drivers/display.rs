use alloc::sync::Arc;
use spin::Once;

use crate::memory::FrameTracker;

/// @description single-scanout adapter 的不可变显示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayMode {
    /// 水平 pixel 数。
    pub(crate) width: u32,
    /// 垂直 pixel 数。
    pub(crate) height: u32,
    /// XRGB8888 每行字节数。
    pub(crate) pitch: u32,
}

/// @description display command 的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayError {
    /// 已有 command 尚未完成，调用方应等待 completion edge。
    WouldBlock,
    /// rectangle 越过当前 scanout。
    InvalidRectangle,
    /// transport、queue 或 response 损坏。
    Device,
}

/// @description deferred display work 对上层发布的单一更新事实。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayUpdate {
    /// 一次 userspace scanout transaction 已完整完成。
    ScanoutCompleted(u64),
    /// adapter 重新查询 display-info 后观察到新的 scanout mode。
    ModeChanged(DisplayMode),
}

/// @description 不泄漏具体 adapter 的 single-scanout display seam。
pub(crate) trait DisplayDevice: Send + Sync {
    /// @description 返回 DRM 已确认提交的当前 scanout mode。
    /// @return 同一代 width、height 与 pitch。
    fn mode(&self) -> DisplayMode;

    /// @description 取得启动期黑屏 scanout backing，供 DRM close/disable 恢复无 owner 状态。
    /// @return adapter 已建立并保活的初始连续 extent。
    fn initial_backing(&self) -> Arc<FrameTracker>;

    /// @description 确认上一次 `ModeChanged` 候选已由 DRM 准备好对应 fallback owner。
    /// @param mode 必须精确等于尚未确认的候选 mode。
    /// @return adapter 与 DRM mode 代际共同提交后返回 unit。
    /// @errors 候选不存在或不匹配返回 `Device`。
    fn commit_mode(&self, mode: DisplayMode) -> Result<(), DisplayError>;

    /// @description 异步把一个连续 XRGB8888 backing 切换为指定 scanout mode。
    /// @param mode 本次 transaction 捕获的 display-info mode。
    /// @param backing 至少覆盖固定 mode pitch × height 的连续物理 extent；adapter 从提交
    /// 到资源解绑完成独立保活该 owner。
    /// @return operation fence；已有 transaction 时返回 `WouldBlock`。
    /// @errors backing 太小返回 `InvalidRectangle`；queue 满、MMIO 或 response 失败返回
    /// `Device`。
    fn submit_scanout(
        &self,
        mode: DisplayMode,
        backing: Arc<FrameTracker>,
    ) -> Result<u64, DisplayError>;

    /// @description 有界消费一个 controlq/config 更新，并推进 transaction state。
    /// @return scanout 最终完成或 mode 改变时返回领域更新；无更新返回 `None`。
    /// @errors descriptor、fence 或 device response 不匹配返回 `Device`。
    fn poll_update(&self) -> Result<Option<DisplayUpdate>, DisplayError>;
}

// OWNER: display facade 唯一持有 DTB 选中的 primary adapter；缺失该 publication 时
// IRQ handler 与后续 DRM fd 会各自决定设备生命周期，scanout backing 可能提前释放。
static PRIMARY_DISPLAY: Once<Arc<dyn DisplayDevice>> = Once::new();

/// @description 发布唯一 primary display adapter。
///
/// @param device 已完成 mode-set 且拥有 scanout backing 的 display adapter。
/// @return 首次发布成功返回 unit。
/// @errors primary display 已存在时返回 unit error。
pub(super) fn register(device: Arc<dyn DisplayDevice>) -> Result<(), ()> {
    if PRIMARY_DISPLAY.get().is_some() {
        return Err(());
    }
    PRIMARY_DISPLAY.call_once(|| device);
    Ok(())
}

/// @description 取得 DTB 选中的唯一 primary display。
/// @return adapter 已发布时返回共享 seam；无 GPU 时返回 `None`。
pub(crate) fn primary_display() -> Option<Arc<dyn DisplayDevice>> {
    PRIMARY_DISPLAY.get().cloned()
}
