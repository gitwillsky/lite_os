/// MMIO 访问错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusError {
    InvalidAddress,
}

/// @description 提供有边界和对齐检查的 MMIO volatile 访问。
pub(crate) struct MmioBus {
    base_addr: usize,
    size: usize,
}

impl MmioBus {
    /// 创建 MMIO 窗口。
    pub(crate) fn new(base_addr: usize, size: usize) -> Result<Self, BusError> {
        if base_addr == 0 || size == 0 || base_addr.checked_add(size).is_none() {
            return Err(BusError::InvalidAddress);
        }
        Ok(Self { base_addr, size })
    }

    fn address(&self, offset: usize, width: usize) -> Result<usize, BusError> {
        let end = offset.checked_add(width).ok_or(BusError::InvalidAddress)?;
        let address = self
            .base_addr
            .checked_add(offset)
            .ok_or(BusError::InvalidAddress)?;
        if end > self.size || address % width != 0 {
            return Err(BusError::InvalidAddress);
        }
        Ok(address)
    }

    pub(crate) fn read_u32(&self, offset: usize) -> Result<u32, BusError> {
        let address = self.address(offset, core::mem::size_of::<u32>())?;
        // SAFETY: `address` 已经边界、溢出和 32 位对齐检查；MMIO 必须 volatile。
        Ok(unsafe { core::ptr::read_volatile(address as *const u32) })
    }

    pub(crate) fn write_u32(&self, offset: usize, value: u32) -> Result<(), BusError> {
        let address = self.address(offset, core::mem::size_of::<u32>())?;
        // SAFETY: `address` 已经边界、溢出和 32 位对齐检查；MMIO 必须 volatile。
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
        Ok(())
    }
}
