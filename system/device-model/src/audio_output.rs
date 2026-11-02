use super::{PcmCapabilities, PcmConfiguration};
use alloc::sync::Arc;

/// 固定 PCM 输出声道数。
pub const PCM_CHANNELS: u8 = 2;
/// 一个 hardware period 的 frame 数。
pub const PCM_PERIOD_FRAMES: usize = 256;
/// hardware buffer 的 period 数。
pub const PCM_PERIODS: usize = 4;
/// 一个 interleaved stereo float frame 的 byte 数。
pub const PCM_FRAME_BYTES: usize = 8;
/// 一个 hardware period 的 byte 数。
pub const PCM_PERIOD_BYTES: usize = PCM_PERIOD_FRAMES * PCM_FRAME_BYTES;
/// 完整 hardware buffer 的 frame 数。
pub const PCM_BUFFER_FRAMES: usize = PCM_PERIOD_FRAMES * PCM_PERIODS;

/// PCM adapter 可恢复操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmOutputError {
    /// adapter 当前所有 period slot 均由 device 持有。
    WouldBlock,
    /// command 与当前 device lifecycle 不匹配。
    InvalidState,
    /// transport、completion 或 device status 已损坏。
    Device,
}

/// PCM completion 进入通用 audio owner 的窄通知 seam。
pub trait PcmCompletionObserver: Send + Sync {
    /// device 完整消费一个 period 后推进 hardware position。
    ///
    /// # Parameters
    ///
    /// - `frames`: 本次完成的固定 frame 数。
    fn period_completed(&self, frames: usize);

    /// device 报告 underrun，当前 stream 进入 XRUN。
    fn xrun(&self);

    /// reset/failure 已不可逆撤销当前 stream。
    fn disconnected(&self);
}

/// 不泄漏 VirtIO queue/config 的通用 PCM playback adapter。
pub trait PcmOutput: Send + Sync {
    /// 设备发现时保存的稳定显示名称。
    fn name(&self) -> &str;
    /// 返回 discovery 得到的实际格式和采样率；不推测宿主 DAC 的能力。
    fn capabilities(&self) -> PcmCapabilities;
    /// 一次性安装 completion observer。
    ///
    /// # Parameters
    ///
    /// - `observer`: 消费服务拥有的 position/readiness sink。
    ///
    /// # Errors
    ///
    /// observer 已安装或 adapter 已失败时返回 `Device`。
    fn set_observer(&self, observer: Arc<dyn PcmCompletionObserver>) -> Result<(), PcmOutputError>;

    /// 按已协商格式配置唯一 playback stream；调用前必须 release 旧配置。
    ///
    /// # Errors
    ///
    /// device 不支持所选格式或 lifecycle 非法时返回错误。
    fn configure(&self, configuration: PcmConfiguration) -> Result<(), PcmOutputError>;

    /// 释放已停止或未启动的物理流；返回前完成所有旧 DMA，之后才能重新配置。
    fn release(&self) -> Result<(), PcmOutputError>;

    /// 准备已配置 stream 与固定 DMA period slots。
    fn prepare(&self) -> Result<(), PcmOutputError>;

    /// 启动已准备或已停止的 stream。
    fn start(&self) -> Result<(), PcmOutputError>;

    /// 停止 stream，并完成全部已经发布的 I/O。
    fn stop(&self) -> Result<(), PcmOutputError>;

    /// 将一个完整的已协商格式 interleaved period 交给 adapter 的有界提交队列。
    ///
    /// 成功后调用者可复用 `bytes`；本地 DMA 或远端传输负责保活数据，完成由 observer 交付。
    /// 每个已配置 adapter 至少容纳 [`PCM_PERIODS`] 个 period。异步设备失败经 `disconnected` 撤销流。
    ///
    /// # Parameters
    ///
    /// - `bytes`: 长度必须恰为当前 configuration 的 period byte 数。
    ///
    /// # Errors
    ///
    /// 无空闲 slot 返回 `WouldBlock`；device/lifecycle 错误按类别返回。
    fn submit_period(&self, bytes: &[u8]) -> Result<(), PcmOutputError>;
}
