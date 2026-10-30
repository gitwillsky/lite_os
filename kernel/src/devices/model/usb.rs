//! USB 身份与安全移除 seam；连接 generation 与 DMA 仍由 host adapter 拥有。
/// 从真实连接读出的 USB 标识；不从声卡号推断 USB 拓扑。
#[derive(Clone, Copy)]
pub(crate) struct UsbIdentity {
    pub(crate) bus: usize,
    pub(crate) port: usize,
    pub(crate) vendor: u16,
    pub(crate) product: u16,
}
/// 移除失败不意味着硬件已经停止访问 DMA。
pub(crate) enum UsbRemoveError {
    Disconnected,
    Device,
}
/// 每次连接独占的设备引用；旧引用不能移除同端口的新连接。
pub(crate) trait UsbDevice: Send + Sync {
    /// 返回连接时的标识。
    fn identity(&self) -> UsbIdentity;
    /// 返回设备显示名。
    fn name(&self) -> &str;
    /// 查询实际 host 状态，不执行控制事务。
    fn connected(&self) -> bool;
    /// 停止连接并等待硬件释放 DMA，成功后才可以物理拔出。
    /// # Errors
    /// 旧连接返回 Disconnected；不能证明硬件已停止时返回 Device。
    fn remove(&self) -> Result<(), UsbRemoveError>;
}
