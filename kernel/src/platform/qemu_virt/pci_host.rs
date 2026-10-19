/// 通用 PCI ECAM host 的设备发现事实；只有存在 PCI host 的机器（UTM 产品拓扑）才有。
///
/// 配置空间枚举与 VirtIO PCI transport 解码属于 `virtio`，这里只给出窗口与 INTx 路由。
#[derive(Debug, Clone)]
pub(crate) struct PciHost {
    /// ECAM 配置空间的 physical 窗口。
    pub(crate) ecam: core::ops::Range<usize>,
    /// 32-bit MMIO BAR 分配窗口。
    pub(crate) mmio32: core::ops::Range<usize>,
    intx: [[u32; 4]; 4],
}

impl PciHost {
    // 只有 AArch64 DTB 解码构造它；RISC-V `virt` 没有 PCI host，`pci_host()` 恒为 `None`。
    #[cfg_attr(
        target_arch = "riscv64",
        allow(dead_code, reason = "RISC-V virt has no PCI host")
    )]
    pub(crate) fn new(
        ecam: core::ops::Range<usize>,
        mmio32: core::ops::Range<usize>,
        intx: [[u32; 4]; 4],
    ) -> Self {
        Self { ecam, mmio32, intx }
    }

    /// 设备 `slot` 的 INTx `pin`（1..=4）路由到的中断 vector；未路由返回 `None`。
    pub(crate) fn interrupt(&self, slot: usize, pin: usize) -> Option<u32> {
        let vector = *self.intx.get(slot)?.get(pin.checked_sub(1)?)?;
        (vector != 0).then_some(vector)
    }
}
