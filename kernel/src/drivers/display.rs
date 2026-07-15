use alloc::sync::Arc;
use spin::Once;

use crate::memory::DeviceBacking;

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

/// @description scanout 坐标系中的半开 damage rectangle。
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
    /// 一次 userspace scanout、damage 或 disable operation 已完整完成。
    OperationCompleted(u64),
    /// adapter 重新查询 display-info 后观察到新的 scanout mode。
    ModeChanged(DisplayMode),
}

/// @description 不泄漏具体 adapter 的 single-scanout display seam。
pub(crate) trait DisplayDevice: Send + Sync {
    /// @description 返回 connector 最新 preferred mode。
    /// @return 同一代 width、height 与 pitch；与 active CRTC mode 相互独立。
    fn mode(&self) -> DisplayMode;

    /// @description 异步把一个 XRGB8888 scatter/gather backing 切换为指定 scanout mode。
    /// @param mode 本次 transaction 捕获的 display-info mode。
    /// @param backing 至少覆盖固定 mode pitch × height；adapter 从提交到资源解绑完成独立
    /// 保活该 owner。
    /// @return operation fence；已有 transaction 时返回 `WouldBlock`。
    /// @errors backing 太小返回 `InvalidRectangle`；queue 满、MMIO 或 response 失败返回
    /// `Device`。
    fn submit_scanout(
        &self,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError>;

    /// @description 把当前 active resource 的若干 damage rectangle 传输并 flush 到 host。
    /// @param rectangles 1..=32 个已合并、非空且位于 active mode 内的 rectangle。
    /// @return blocking DIRTYFB 等待的 operation fence。
    /// @errors 无 active resource、rectangle 越界、已有 operation 或 device failure。
    fn submit_damage(&self, rectangles: &[DisplayRect]) -> Result<u64, DisplayError>;

    /// @description 以标准 resource_id=0 禁用 scanout，再解绑并释放 active resource。
    /// @return disable operation fence；hardware 不再引用 backing 后才完成。
    /// @errors 无 active resource、已有 operation 或 device failure。
    fn disable_scanout(&self) -> Result<u64, DisplayError>;

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
