use core::mem::MaybeUninit;

/// entropy 设备失败、请求存储不足，或没有任何 entropy source；不生成伪随机 fallback。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntropyError;

/// 硬件 entropy source seam。
pub trait EntropySource: Send + Sync {
    /// 用设备 entropy 完整初始化 `bytes`；可阻塞当前 task 直到设备完成。
    ///
    /// # Errors
    ///
    /// 设备失败或 request 存储不足返回 [`EntropyError`]，`bytes` 内容未定义。
    fn fill(&self, bytes: &mut [MaybeUninit<u8>]) -> Result<(), EntropyError>;
}
