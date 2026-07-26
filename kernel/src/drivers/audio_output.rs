use alloc::sync::Arc;

/// 固定 PCM 输出正规形。
pub(crate) const PCM_RATE: u32 = 48_000;
/// 固定 PCM 输出声道数。
pub(crate) const PCM_CHANNELS: u8 = 2;
/// 一个 hardware period 的 frame 数。
pub(crate) const PCM_PERIOD_FRAMES: usize = 256;
/// hardware buffer 的 period 数。
pub(crate) const PCM_PERIODS: usize = 4;
/// 一个 interleaved stereo float frame 的 byte 数。
pub(crate) const PCM_FRAME_BYTES: usize = 8;
/// 一个 hardware period 的 byte 数。
pub(crate) const PCM_PERIOD_BYTES: usize = PCM_PERIOD_FRAMES * PCM_FRAME_BYTES;
/// 完整 hardware buffer 的 frame 数。
pub(crate) const PCM_BUFFER_FRAMES: usize = PCM_PERIOD_FRAMES * PCM_PERIODS;
/// 完整 hardware buffer 的 byte 数。
pub(crate) const PCM_BUFFER_BYTES: usize = PCM_PERIOD_BYTES * PCM_PERIODS;

/// PCM adapter 可恢复操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PcmOutputError {
    /// adapter 当前所有 period slot 均由 device 持有。
    WouldBlock,
    /// command 与当前 device lifecycle 不匹配。
    InvalidState,
    /// transport、completion 或 device status 已损坏。
    Device,
}

/// PCM completion 进入通用 audio owner 的窄通知 seam。
pub(crate) trait PcmCompletionObserver: Send + Sync {
    /// @description device 完整消费一个 period 后推进 hardware position。
    /// @param frames 本次完成的固定 frame 数。
    fn period_completed(&self, frames: usize);

    /// @description device 报告 underrun，当前 stream 进入 XRUN。
    fn xrun(&self);

    /// @description reset/failure 已不可逆撤销当前 stream。
    fn disconnected(&self);
}

/// 不泄漏 VirtIO queue/config 的通用 PCM playback adapter。
pub(crate) trait PcmOutput: Send + Sync {
    /// @description 一次性安装 completion observer。
    /// @param observer `kernel::audio` 拥有的 position/readiness sink。
    /// @errors observer 已安装或 adapter 已失败时返回 `Device`。
    fn set_observer(&self, observer: Arc<dyn PcmCompletionObserver>) -> Result<(), PcmOutputError>;

    /// @description 按固定正规形配置唯一 playback stream。
    /// @errors device 不支持固定格式或 lifecycle 非法时返回错误。
    fn configure(&self) -> Result<(), PcmOutputError>;

    /// @description 准备已配置 stream 与固定 DMA period slots。
    fn prepare(&self) -> Result<(), PcmOutputError>;

    /// @description 启动已准备或已停止的 stream。
    fn start(&self) -> Result<(), PcmOutputError>;

    /// @description 停止 stream，并完成全部已经发布的 I/O。
    fn stop(&self) -> Result<(), PcmOutputError>;

    /// @description 释放 device stream resources，回到可重新配置状态。
    fn release(&self) -> Result<(), PcmOutputError>;

    /// @description 发布一个完整 256-frame interleaved float period。
    /// @param bytes 长度必须恰为 [`PCM_PERIOD_BYTES`]。
    /// @errors 无空闲 slot 返回 `WouldBlock`；device/lifecycle 错误按类别返回。
    fn submit_period(&self, bytes: &[u8]) -> Result<(), PcmOutputError>;

    /// @description 当前至少有一个可提交 period slot 时返回 true。
    fn writable(&self) -> bool;
}
