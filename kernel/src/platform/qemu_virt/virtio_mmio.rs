//! QEMU `virt` VirtIO-MMIO transport 的 DTB 发现结果。
//!
//! transport 身份只由 `compatible = "virtio,mmio"` 裁决。节点名只是 QEMU 的命名约定：QEMU 11 已把
//! PLIC 改名为 `interrupt-controller@…`，按名称前缀匹配会让设备在升级后静默消失。

use core::ops::Range;

use dtb_walker::{Str, StrList};

/// QEMU `virt` 两个架构公开的 VirtIO-MMIO transport 上限（AArch64 为 32 个 slot）。
const MAX_TRANSPORTS: usize = 32;

/// 一个 VirtIO-MMIO transport 的 physical window 与 platform interrupt vector。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VirtioMmioTransport {
    pub(crate) base_addr: usize,
    pub(crate) size: usize,
    pub(crate) irq: u32,
}

/// 按 DTB 顺序记录的 transport 表；容量满时 fail-stop，不静默丢弃设备。
#[derive(Debug, Clone, Copy)]
pub(crate) struct VirtioMmioTransports {
    devices: [Option<VirtioMmioTransport>; MAX_TRANSPORTS],
    count: usize,
}

impl VirtioMmioTransports {
    /// 返回按 DTB 顺序排列的全部 transport。
    pub(crate) fn iter(&self) -> impl Iterator<Item = &VirtioMmioTransport> {
        self.devices[..self.count].iter().flatten()
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    /// 覆盖全部 transport window 的最小 physical 区间；没有 transport 时返回 `None`。
    pub(crate) fn span(&self) -> Option<Range<usize>> {
        let start = self.iter().map(|device| device.base_addr).min()?;
        let end = self
            .iter()
            .map(|device| {
                device
                    .base_addr
                    .checked_add(device.size)
                    .expect("validated VirtIO MMIO range overflowed")
            })
            .max()?;
        Some(start..end)
    }
}

/// 当前 DTB 节点的 VirtIO 候选属性。
///
/// DTB 规范要求节点的全部属性先于其子节点出现，因此下一个 `SubNode` 事件或遍历结束时，
/// 上一个节点的属性已经完整；扫描器在这两个时机提交，结果与属性顺序无关。
#[derive(Default)]
struct PendingNode {
    compatible: bool,
    reg: Option<Range<usize>>,
    irq: Option<u32>,
}

/// 由各架构 DTB parser 驱动的 VirtIO-MMIO 识别器。
///
/// 1. 每个 `SubNode` 事件调用 [`Self::begin_node`]，提交上一个节点并清空候选；
/// 2. 节点的 `compatible`、首个 `reg` 与首个已解码 interrupt 依次喂入；
/// 3. 遍历结束后 [`Self::finish`] 提交最后一个节点并返回 transport 表。
pub(crate) struct VirtioMmioScan {
    transports: VirtioMmioTransports,
    pending: PendingNode,
}

impl VirtioMmioScan {
    pub(crate) const fn new() -> Self {
        Self {
            transports: VirtioMmioTransports {
                devices: [None; MAX_TRANSPORTS],
                count: 0,
            },
            pending: PendingNode {
                compatible: false,
                reg: None,
                irq: None,
            },
        }
    }

    /// 进入新节点前提交上一个节点。
    ///
    /// # Panics
    ///
    /// 上一个节点声明 `virtio,mmio` 却缺少合法 `reg`/`interrupts`，或 transport 超过
    /// [`MAX_TRANSPORTS`] 时 fail-stop；静默跳过会让对应设备在运行时无声缺失。
    pub(crate) fn begin_node(&mut self) {
        let pending = core::mem::take(&mut self.pending);
        if !pending.compatible {
            return;
        }
        let range = pending
            .reg
            .filter(|range| range.start != 0 && range.end > range.start)
            .expect("virtio,mmio node lacks a valid reg window");
        let irq = pending.irq.expect("virtio,mmio node lacks an interrupt");
        let transports = &mut self.transports;
        assert!(
            transports.count < MAX_TRANSPORTS,
            "DTB declares more than {MAX_TRANSPORTS} VirtIO-MMIO transports"
        );
        transports.devices[transports.count] = Some(VirtioMmioTransport {
            base_addr: range.start,
            size: range.end - range.start,
            irq,
        });
        transports.count += 1;
    }

    pub(crate) fn compatible(&mut self, mut values: StrList<'_>) {
        self.pending.compatible |= values.any(|value| value == Str::from("virtio,mmio"));
    }

    pub(crate) fn reg(&mut self, range: Option<Range<usize>>) {
        self.pending.reg = self.pending.reg.take().or(range);
    }

    pub(crate) fn interrupt(&mut self, irq: Option<u32>) {
        self.pending.irq = self.pending.irq.or(irq);
    }

    pub(crate) fn finish(mut self) -> VirtioMmioTransports {
        self.begin_node();
        self.transports
    }
}
