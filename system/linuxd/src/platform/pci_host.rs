/// 通用 PCI ECAM host 的设备发现事实。
///
/// 配置空间枚举与 BAR 分配属于 `pci`，具体 transport 由 adapter 解码；这里只给出窗口、INTx 路由
/// 与 IOMMU stream 映射。
#[derive(Debug, Clone)]
pub(crate) struct PciHost {
    /// ECAM 配置空间的 physical 窗口。
    pub(crate) ecam: core::ops::Range<usize>,
    /// 32-bit MMIO BAR 分配窗口。
    pub(crate) mmio32: core::ops::Range<usize>,
    intx: [[u32; 4]; 4],
    /// `iommu-map` 首项：(RID 基址, stream 基址, 长度)；没有 IOMMU 时为 `None`。
    stream_map: Option<(u32, u32, u32)>,
}

impl PciHost {
    pub(crate) fn new(
        ecam: core::ops::Range<usize>,
        mmio32: core::ops::Range<usize>,
        intx: [[u32; 4]; 4],
        stream_map: Option<(u32, u32, u32)>,
    ) -> Self {
        Self {
            ecam,
            mmio32,
            intx,
            stream_map,
        }
    }

    /// 设备 `slot` 的 INTx `pin`（1..=4）路由到的中断 vector；未路由返回 `None`。
    pub(crate) fn interrupt(&self, slot: usize, pin: usize) -> Option<u32> {
        let vector = *self.intx.get(slot & 3)?.get(pin.checked_sub(1)?)?;
        (vector != 0).then_some(vector)
    }

    /// 请求者 ID（`bus << 8 | device << 3 | function`）对应的 IOMMU stream；
    /// 不经 IOMMU 时返回 `None`。
    pub(crate) fn stream(&self, requester: u32) -> Option<u32> {
        let (base, stream_base, length) = self.stream_map?;
        let offset = requester.checked_sub(base)?;
        (offset < length).then(|| stream_base + offset)
    }
}
