//! @description Kernel entropy facade and bounded initialized output ownership.

use alloc::{boxed::Box, vec::Vec};
use core::mem::MaybeUninit;

/// @description 唯一 entropy device source 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RandomError {
    /// entropy device 未注册、失败或返回非法 completion。
    DeviceUnavailable,
}

/// @description 有上限的 heap-backed entropy output owner。
///
/// `CAPACITY` 在 call site 固定最大批次；allocation 不占 kernel stack，bytes 仅在 driver
/// 成功完整初始化后投影为 `u8`。Drop 自动释放尚未初始化或已初始化的同一 allocation。
pub(crate) struct EntropyBatch<const CAPACITY: usize> {
    bytes: Box<[MaybeUninit<u8>]>,
}

impl<const CAPACITY: usize> EntropyBatch<CAPACITY> {
    /// @description 分配一个未初始化的 bounded entropy batch。
    ///
    /// @return 成功时返回容量恰为 `CAPACITY` 的 heap owner；allocation failure 返回 `None`。
    pub(crate) fn try_new() -> Option<Self> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(CAPACITY).ok()?;
        // SAFETY: `MaybeUninit<u8>` accepts every bit pattern, and capacity is at least CAPACITY.
        // No element is projected as u8 until `fill` has initialized the requested prefix.
        unsafe { bytes.set_len(CAPACITY) };
        Some(Self {
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// @description 由唯一 entropy source 初始化并返回一个 prefix。
    ///
    /// @param length 请求字节数，必须不大于 `CAPACITY`。
    /// @return device 完整初始化成功时返回恰好 `length` 字节的只读 slice。
    /// @errors device 未注册、失败或 completion 非法时返回 `DeviceUnavailable`。
    pub(crate) fn fill(&mut self, length: usize) -> Result<&[u8], RandomError> {
        assert!(length <= CAPACITY, "entropy batch length exceeds capacity");
        crate::drivers::fill_entropy(&mut self.bytes[..length])
            .map_err(|_| RandomError::DeviceUnavailable)?;
        // SAFETY: `fill_entropy` returns Ok only after writing every element of this exact prefix.
        // Missing that contract would expose uninitialized bytes to user memory and be UB.
        Ok(unsafe { core::slice::from_raw_parts(self.bytes.as_ptr().cast::<u8>(), length) })
    }
}
