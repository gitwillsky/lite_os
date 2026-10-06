//! named byte-stream port seam（VirtIO Console multiport 等）。

/// VirtIO port byte-stream operation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortError {
    /// No byte or transmit slot is currently available.
    WouldBlock,
    /// The selected named port is closed or the transport failed.
    Disconnected,
}

/// One deferred VirtIO Console drain result.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PortActivity {
    /// Receive bytes or disconnect state changed.
    pub(crate) readable_changed: bool,
    /// A transmit slot became available or the port disconnected.
    pub(crate) writable_changed: bool,
    /// At least one queue still contains a completion after the bounded pass.
    pub(crate) backlog: bool,
}

/// 一个已选中的 named byte-stream port；读写不睡眠，阻塞由消费领域负责。
pub(crate) trait PortDevice: Send + Sync {
    /// Read buffered bytes without sleeping.
    ///
    /// # Errors
    ///
    /// No byte is available returns `WouldBlock`; a closed port returns `Disconnected`.
    fn read(&self, output: &mut [u8]) -> Result<usize, PortError>;

    /// Submit one bounded byte-stream fragment without sleeping.
    ///
    /// # Errors
    ///
    /// No transmit slot returns `WouldBlock`; a closed port returns `Disconnected`.
    fn write(&self, input: &[u8]) -> Result<usize, PortError>;

    /// Receive bytes are buffered or the port disconnected.
    fn readable(&self) -> bool;

    /// A transmit slot is available or the port disconnected.
    fn writable(&self) -> bool;

    /// Both guest and host opened the selected port.
    fn connected(&self) -> bool;

    /// Drain a bounded completion batch at a safe point.
    fn dispatch(&self) -> PortActivity;

    /// 绑定消费者分配的 completion deferred vector；绑定前到达的 completion 由绑定时的一次
    /// 发布补偿。
    fn bind_completion_work(&self, work: crate::cpu::DeferredWork);
}
