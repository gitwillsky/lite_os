//! memfd seal 策略：纯状态机，只裁决操作是否允许，不拥有任何数据。

pub(super) const F_SEAL_SEAL: u32 = 0x0001;
pub(super) const F_SEAL_SHRINK: u32 = 0x0002;
pub(super) const F_SEAL_GROW: u32 = 0x0004;
pub(super) const F_SEAL_WRITE: u32 = 0x0008;
pub(super) const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
const SUPPORTED_SEALS: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW;
const KNOWN_SEALS: u32 = SUPPORTED_SEALS | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SealError {
    /// 请求了未知或尚未实现的 seal。
    InvalidOperation,
    /// 已被 seal 禁止。
    PermissionDenied,
}

/// 一个内存型文件当前生效的 seal mask。
#[derive(Debug, Clone, Copy)]
pub(super) struct Seals(u32);

impl Seals {
    /// 构造初始 seal 状态。
    ///
    /// # Parameters
    ///
    /// - `allow_sealing`: 未设置 `MFD_ALLOW_SEALING`（以及普通 tmpfs 文件）时初始带
    ///   `F_SEAL_SEAL`，之后不能再添加任何 seal。
    pub(super) const fn new(allow_sealing: bool) -> Self {
        Self(if allow_sealing { 0 } else { F_SEAL_SEAL })
    }

    pub(super) const fn bits(self) -> u32 {
        self.0
    }

    /// 追加受支持的 seal。
    ///
    /// # Returns
    ///
    /// 新 seal mask。
    ///
    /// # Errors
    ///
    /// 未知 seal 或要求 `F_SEAL_WRITE`/`F_SEAL_FUTURE_WRITE`（尚未实现）返回 `InvalidOperation`；
    /// 已带 `F_SEAL_SEAL` 返回 `PermissionDenied`。
    pub(super) fn add(&mut self, seals: u32) -> Result<u32, SealError> {
        if seals & !KNOWN_SEALS != 0 || seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
            return Err(SealError::InvalidOperation);
        }
        if self.0 & F_SEAL_SEAL != 0 {
            return Err(SealError::PermissionDenied);
        }
        self.0 |= seals;
        Ok(self.0)
    }

    /// 写入 `[.., end)` 范围是否允许：`F_SEAL_GROW` 禁止超过当前长度。
    pub(super) fn check_write(self, length: u64, end: u64) -> Result<(), SealError> {
        if self.0 & F_SEAL_GROW != 0 && end > length {
            return Err(SealError::PermissionDenied);
        }
        Ok(())
    }

    /// 把长度改为 `size` 是否允许：`F_SEAL_SHRINK`/`F_SEAL_GROW` 分别禁止缩小与增长。
    pub(super) fn check_truncate(self, length: u64, size: u64) -> Result<(), SealError> {
        if size < length && self.0 & F_SEAL_SHRINK != 0
            || size > length && self.0 & F_SEAL_GROW != 0
        {
            return Err(SealError::PermissionDenied);
        }
        Ok(())
    }
}
