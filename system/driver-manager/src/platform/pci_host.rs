use alloc::vec::Vec;

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
    stream: RequesterMap,
    messages: MessageRouting,
}

/// PCI MSI 的标准固件路由；ITS 做 RID→DeviceID，IMSIC 的 msi-parent 直连不做硬件 ID 翻译。
#[derive(Debug, Clone)]
pub(crate) enum MessageRouting {
    None,
    Remapped(RequesterMap),
    /// 单 PCI domain 的 RID 作为软件分配租约身份；不冒充 IMSIC 硬件 DeviceID 或 DMA stream。
    Direct,
}

impl PciHost {
    pub(crate) fn new(
        ecam: core::ops::Range<usize>,
        mmio32: core::ops::Range<usize>,
        intx: [[u32; 4]; 4],
        stream: RequesterMap,
        messages: MessageRouting,
    ) -> Self {
        Self {
            ecam,
            mmio32,
            intx,
            stream,
            messages,
        }
    }

    /// 设备 `slot` 的 INTx `pin`（1..=4）路由到的中断 vector；未路由返回 `None`。
    pub(crate) fn interrupt(&self, slot: usize, pin: usize) -> Option<u32> {
        let vector = *self.intx.get(slot & 3)?.get(pin.checked_sub(1)?)?;
        (vector != 0).then_some(vector)
    }

    /// 请求者 ID（`bus << 8 | device << 3 | function`）对应的 IOMMU stream；
    /// 没有 iommu-map 时返回 `None`；声明了映射却未覆盖 requester 时 fail-stop，禁止隐式绕过。
    pub(crate) fn stream(&self, requester: u32) -> Option<u32> {
        self.stream.translate(requester)
    }

    /// 消息组的设备身份：ITS 使用 msi-map 的 DeviceID，IMSIC 直连使用独立的软件 RID 租约。
    /// 没有标准消息路由时返回 None，不从 DMA stream 猜测。
    pub(crate) fn message_device(&self, requester: u32) -> Option<u32> {
        match &self.messages {
            MessageRouting::None => None,
            MessageRouting::Remapped(map) => map.translate(requester),
            MessageRouting::Direct => Some(requester),
        }
    }
}

/// PCI requester ID→目标 ID 的标准区间映射；IOMMU 与 MSI 各持有独立实例，共用校验与投影规则。
#[derive(Debug, Clone)]
pub(crate) struct RequesterMap {
    ranges: Vec<(u32, u32, u32)>,
    mask: u32,
}
impl RequesterMap {
    /// 校验 phandle 已解析的映射；范围、mask 或重叠非法时 fail-stop。
    pub(crate) fn new(ranges: Vec<(u32, u32, u32)>, mask: u32) -> Self {
        for (index, &(base, target, length)) in ranges.iter().enumerate() {
            assert!(
                length != 0
                    && base & !mask == 0
                    && base.checked_add(length - 1).is_some()
                    && target.checked_add(length - 1).is_some(),
                "invalid PCI requester map range/mask"
            );
            assert!(
                !ranges[..index]
                    .iter()
                    .any(
                        |&(old, _, count)| u64::from(base) < u64::from(old) + u64::from(count)
                            && u64::from(old) < u64::from(base) + u64::from(length)
                    ),
                "overlapping PCI requester map ranges"
            );
        }
        Self { ranges, mask }
    }
    /// 无映射返回 None；声明了映射却未覆盖 requester 时 fail-stop，不能静默绕过 authority。
    fn translate(&self, requester: u32) -> Option<u32> {
        if self.ranges.is_empty() {
            return None;
        }
        let requester = requester & self.mask;
        self.ranges
            .iter()
            .find_map(|&(base, target, length)| {
                let offset = requester.checked_sub(base)?;
                (offset < length).then(|| target + offset)
            })
            .map(Some)
            .expect("PCI requester is not covered by its declared map")
    }
}
