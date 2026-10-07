/// MMIO 访问错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusError {
    InvalidAddress,
}

/// 提供有边界和对齐检查、并由静态 arch façade 固定指令形态的 MMIO 访问。
#[derive(Debug, Clone, Copy)]
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
        if width > self.size || offset > self.size - width {
            return Err(BusError::InvalidAddress);
        }
        // 构造器已证明 base + size 不溢出；offset < size，因此无需重复检查 base + offset。
        let address = self.base_addr + offset;
        if !address.is_multiple_of(width) {
            return Err(BusError::InvalidAddress);
        }
        Ok(address)
    }

    /// 从 MMIO window 读取一个 byte。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 window base 的 byte offset。
    ///
    /// # Returns
    ///
    /// volatile 读取值。
    ///
    /// # Errors
    ///
    /// offset 越界返回 `InvalidAddress`。
    pub(crate) fn read_u8(&self, offset: usize) -> Result<u8, BusError> {
        let address = self.address(offset, core::mem::size_of::<u8>())?;
        // SAFETY: `address` 已由本 window 完成范围检查；arch owner 保证单次 device access。
        Ok(unsafe { crate::arch::read_mmio_u8(address) })
    }

    /// 向 MMIO window 写入一个 byte。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 window base 的 byte offset。
    /// - `value`: 要发布的 byte。
    ///
    /// # Returns
    ///
    /// 写入成功返回 unit。
    ///
    /// # Errors
    ///
    /// offset 越界返回 `InvalidAddress`。
    pub(crate) fn write_u8(&self, offset: usize, value: u8) -> Result<(), BusError> {
        let address = self.address(offset, core::mem::size_of::<u8>())?;
        // SAFETY: `address` 已由本 window 完成范围检查；arch owner 保证单次 device access。
        unsafe { crate::arch::write_mmio_u8(address, value) };
        Ok(())
    }

    /// 从 MMIO window 读取一个 little-endian 16-bit halfword。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 window base 的 byte offset。
    ///
    /// # Returns
    ///
    /// volatile 读取值。
    ///
    /// # Errors
    ///
    /// offset 越界或未按 16-bit 对齐时返回 `InvalidAddress`。
    pub(crate) fn read_u16(&self, offset: usize) -> Result<u16, BusError> {
        let address = self.address(offset, core::mem::size_of::<u16>())?;
        // SAFETY: `address` 已完成边界、溢出与 16 位对齐检查。
        Ok(unsafe { crate::arch::read_mmio_u16(address) })
    }

    /// 向 MMIO window 写入一个 little-endian 16-bit halfword。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 window base 的 byte offset。
    /// - `value`: 要发布的 halfword。
    ///
    /// # Returns
    ///
    /// 写入成功返回 unit。
    ///
    /// # Errors
    ///
    /// offset 越界或未按 16-bit 对齐时返回 `InvalidAddress`。
    pub(crate) fn write_u16(&self, offset: usize, value: u16) -> Result<(), BusError> {
        let address = self.address(offset, core::mem::size_of::<u16>())?;
        // SAFETY: `address` 已完成边界、溢出与 16 位对齐检查。
        unsafe { crate::arch::write_mmio_u16(address, value) };
        Ok(())
    }

    /// 读取一个 32-bit 寄存器。
    ///
    /// # Errors
    ///
    /// offset 越界或未按 32-bit 对齐时返回 `InvalidAddress`。
    pub(crate) fn read_u32(&self, offset: usize) -> Result<u32, BusError> {
        let address = self.address(offset, core::mem::size_of::<u32>())?;
        // SAFETY: `address` 已完成边界、溢出与 32 位对齐检查。
        Ok(unsafe { crate::arch::read_mmio_u32(address) })
    }

    /// 写入一个 32-bit 寄存器。
    ///
    /// # Errors
    ///
    /// offset 越界或未按 32-bit 对齐时返回 `InvalidAddress`。
    pub(crate) fn write_u32(&self, offset: usize, value: u32) -> Result<(), BusError> {
        let address = self.address(offset, core::mem::size_of::<u32>())?;
        // SAFETY: `address` 已完成边界、溢出与 32 位对齐检查。
        unsafe { crate::arch::write_mmio_u32(address, value) };
        Ok(())
    }

    /// 读取一个 64-bit 寄存器。
    ///
    /// # Errors
    ///
    /// offset 越界或未按 64-bit 对齐时返回 `InvalidAddress`。
    #[allow(
        dead_code,
        reason = "used by the AArch64 GICv3 frames; RISC-V PLIC has 32-bit registers only"
    )]
    pub(crate) fn read_u64(&self, offset: usize) -> Result<u64, BusError> {
        let address = self.address(offset, core::mem::size_of::<u64>())?;
        // SAFETY: `address` 已完成边界、溢出与 64 位对齐检查。
        Ok(unsafe { crate::arch::read_mmio_u64(address) })
    }

    /// 写入一个 64-bit 寄存器。
    ///
    /// # Errors
    ///
    /// offset 越界或未按 64-bit 对齐时返回 `InvalidAddress`。
    #[allow(
        dead_code,
        reason = "used by the AArch64 GICv3 frames; RISC-V PLIC has 32-bit registers only"
    )]
    pub(crate) fn write_u64(&self, offset: usize, value: u64) -> Result<(), BusError> {
        let address = self.address(offset, core::mem::size_of::<u64>())?;
        // SAFETY: `address` 已完成边界、溢出与 64 位对齐检查。
        unsafe { crate::arch::write_mmio_u64(address, value) };
        Ok(())
    }

    /// 取出本 window 内 `[offset, offset + size)` 的子 window（例如一个 redistributor frame）。
    ///
    /// # Errors
    ///
    /// 子区间越界或溢出时返回 `InvalidAddress`。
    #[allow(
        dead_code,
        reason = "used by the AArch64 GICv3 frames; RISC-V PLIC has 32-bit registers only"
    )]
    pub(crate) fn subwindow(&self, offset: usize, size: usize) -> Result<Self, BusError> {
        let end = offset.checked_add(size).ok_or(BusError::InvalidAddress)?;
        if end > self.size {
            return Err(BusError::InvalidAddress);
        }
        let base = self
            .base_addr
            .checked_add(offset)
            .ok_or(BusError::InvalidAddress)?;
        Self::new(base, size)
    }
}

/// 在发出设备 doorbell 之前排序此前对 DMA 内存的写入（arch 的 MMIO write 屏障）。
///
/// 缺失时设备可能在 doorbell 之后读到未写完的 descriptor。
pub(crate) fn before_mmio_write() {
    crate::arch::before_mmio_write();
}
