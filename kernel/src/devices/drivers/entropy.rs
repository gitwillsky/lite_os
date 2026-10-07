use core::mem::MaybeUninit;

/// 硬件 entropy source seam。
pub(crate) trait EntropySource: Send + Sync {
    /// 用设备 entropy 完整初始化 `bytes`；可阻塞当前 task 直到设备完成。
    ///
    /// # Errors
    ///
    /// 设备失败或 request 存储不足返回 unit error，`bytes` 内容未定义。
    fn fill(&self, bytes: &mut [MaybeUninit<u8>]) -> Result<(), ()>;
}
