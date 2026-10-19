//! Linux/riscv64 rt_sigaction 的架构布局；通用 syscall 不解释 restorer offset。

/// 固定 24-byte 的 kernel sigaction user-copy image。
pub(crate) struct SignalActionBytes {
    bytes: [u8; 24],
}

impl SignalActionBytes {
    /// 已实现的 Linux signal behavior flags；SA_UNSUPPORTED/未知位清零支持标准探测。
    pub(crate) const fn supported_flags() -> usize {
        0xd8000007
    }

    /// 构造待 copyin 的零 image。
    pub(crate) const fn zeroed() -> Self {
        Self { bytes: [0; 24] }
    }
    /// 返回完整 copyin buffer。
    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
    /// 返回完整 copyout image。
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// 解码 handler、flags、restorer 与标准八字节 mask。
    pub(crate) fn decode(&self) -> (usize, usize, usize, u64) {
        let word = |offset| {
            usize::from_ne_bytes(
                self.bytes[offset..offset + 8]
                    .try_into()
                    .expect("fixed sigaction field"),
            )
        };
        (word(0), word(8), 0, word(16) as u64)
    }
    /// 编码完整 action，所有未使用字节保持为零。
    pub(crate) fn encode(handler: usize, flags: usize, _restorer: usize, mask: u64) -> Self {
        let mut image = Self::zeroed();
        image.bytes[..8].copy_from_slice(&handler.to_ne_bytes());
        image.bytes[8..16].copy_from_slice(&flags.to_ne_bytes());
        image.bytes[16..24].copy_from_slice(&mask.to_ne_bytes());
        image
    }
}
