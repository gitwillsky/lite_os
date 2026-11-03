//! 本宿主节点的 PCI function：配置空间、已分配的 memory BAR 与 INTx；枚举与分配属于 driver-manager。
use launch_protocol::driver_host::Bus;
use mmio::MmioBus;

/// 已分配且互不重叠的 memory BAR。
#[derive(Clone, Copy)]
pub(crate) struct Bar {
    pub(crate) address: usize,
    pub(crate) size: usize,
}

/// 节点的 PCI function；配置与 BAR 已由 driver-manager 写入。
pub(crate) struct Function {
    pub(crate) config: MmioBus,
    pub(crate) bars: [Option<Bar>; 6],
    pub(crate) interrupt: Option<u32>,
    /// 本 function 发起 DMA 使用的 BTI。
    pub(crate) bti: &'static native_runtime::Bti,
}

/// 映射节点的 PCI function。
///
/// # Returns
///
/// 节点不在 PCI 总线上时为 `None`。
pub(crate) fn function() -> Option<Function> {
    let Bus::Pci {
        config,
        bars,
        interrupt,
    } = crate::platform::node().bus
    else {
        return None;
    };
    let config = MmioBus::new(
        crate::platform::map_device_window(config as usize, 4096),
        4096,
    )
    .ok()?;
    Some(Function {
        config,
        bars: bars.map(|bar| {
            bar.map(|bar| Bar {
                address: bar.address as usize,
                size: bar.size as usize,
            })
        }),
        interrupt,
        bti: crate::platform::bti(),
    })
}
