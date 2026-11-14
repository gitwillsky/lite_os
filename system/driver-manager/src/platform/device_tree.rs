//! 从平台 DTB 提取 driver-host 驱动所需的设备事实。
//!
//! 只关心由用户态驱动拥有的设备：VirtIO-MMIO transport、通用 PCI ECAM host 与 IOMMU。
//! 中断控制器、串口、RTC 与定时器由内核拥有，这里不解析。中断说明符按架构解码：
//! AArch64 为 GICv3 三 cell（SPI 号 + 32），RISC-V 为 APLIC 两 cell（source 号与触发标志）。

use alloc::vec::Vec;
use core::ops::Range;

use dtb_walker::{Dtb, DtbObj, Property, Str, WalkOperation};

use super::pci_host::{MessageRouting, PciHost, RequesterMap};
use super::virtio_mmio::{VirtioMmioScan, VirtioMmioTransports};

/// 设备树中的 IOMMU。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IommuFacts {
    /// `native_abi::iommu::*`。
    pub(crate) kind: u32,
    pub(crate) base: usize,
    pub(crate) size: usize,
    phandle: u32,
}

/// driver-host 使用的设备事实。
pub(crate) struct DeviceFacts {
    pub(crate) virtio: VirtioMmioTransports,
    pub(crate) pci: Option<PciHost>,
    pub(crate) iommu: Option<IommuFacts>,
}

/// 中断说明符的 cell 数。
#[cfg(target_arch = "aarch64")]
const INTERRUPT_CELLS: usize = 3;
#[cfg(target_arch = "riscv64")]
const INTERRUPT_CELLS: usize = 2;
#[cfg(target_arch = "aarch64")]
const MESSAGE_COMPATIBLE: &str = "arm,gic-v3-its";
#[cfg(target_arch = "aarch64")]
const MESSAGE_CELLS: u32 = 1;
#[cfg(target_arch = "riscv64")]
const MESSAGE_COMPATIBLE: &str = "riscv,imsics";
#[cfg(target_arch = "riscv64")]
const MESSAGE_CELLS: u32 = 0;
const GIC_V2M_COMPATIBLE: &str = "arm,gic-v2m-frame";
/// PCI `interrupt-map` 每项中父中断控制器的地址 cell 数（GICv3 为 2，APLIC 为 0）。
#[cfg(target_arch = "aarch64")]
const PARENT_ADDRESS_CELLS: usize = 2;
#[cfg(target_arch = "riscv64")]
const PARENT_ADDRESS_CELLS: usize = 0;

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("DTB cell must contain four bytes"))
}

fn be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(
        bytes
            .try_into()
            .expect("DTB value must contain eight bytes"),
    )
}

/// 解码一个中断说明符为平台中断向量。
fn decode_interrupt(cells: &[u8]) -> Option<u32> {
    let cells = cells.get(..INTERRUPT_CELLS * 4)?;
    #[cfg(target_arch = "aarch64")]
    {
        let number = be_u32(&cells[4..8]);
        match be_u32(&cells[..4]) {
            0 => number.checked_add(32),
            1 => number.checked_add(16),
            _ => None,
        }
    }
    #[cfg(target_arch = "riscv64")]
    {
        assert_eq!(
            be_u32(&cells[4..8]),
            4,
            "driver nodes require level-high APLIC interrupts"
        );
        Some(be_u32(&cells[..4])).filter(|&source| source != 0)
    }
}

fn decode_pci_mmio32(bytes: &[u8]) -> Option<Range<usize>> {
    for cells in bytes.as_chunks::<28>().0 {
        if be_u32(&cells[0..4]) & 0x0300_0000 != 0x0200_0000 {
            continue;
        }
        let child = be_u64(&cells[4..12]);
        let parent = be_u64(&cells[12..20]);
        let size = be_u64(&cells[20..28]);
        if child != parent {
            continue;
        }
        let start = usize::try_from(parent).ok()?;
        return Some(start..start.checked_add(usize::try_from(size).ok()?)?);
    }
    None
}

fn decode_pci_interrupt_map(bytes: &[u8], routes: &mut [[u32; 4]; 4]) {
    // 子单元地址 3 cell + 子中断 1 cell + phandle + 父地址 + 父中断说明符。
    let entry = (3 + 1 + 1 + PARENT_ADDRESS_CELLS + INTERRUPT_CELLS) * 4;
    for cells in bytes.chunks_exact(entry) {
        let slot = ((be_u32(&cells[0..4]) >> 11) & 0x1f) as usize;
        let pin = be_u32(&cells[12..16]) as usize;
        let Some(vector) = decode_interrupt(&cells[(5 + PARENT_ADDRESS_CELLS) * 4..]) else {
            continue;
        };
        if let Some(route) = routes
            .get_mut(slot)
            .and_then(|pins| pins.get_mut(pin.wrapping_sub(1)))
        {
            *route = vector;
        }
    }
}

fn iommu_kind(compatible: dtb_walker::StrList<'_>) -> Option<u32> {
    let mut kind = None;
    for value in compatible {
        if value == Str::from("arm,smmu-v3") {
            kind = Some(native_runtime::native_abi::iommu::ARM_SMMU_V3);
        } else if value == Str::from("riscv,iommu") {
            kind = Some(native_runtime::native_abi::iommu::RISCV);
        }
    }
    kind
}

#[derive(Default)]
struct PciScan {
    is_host: bool,
    ecam: Option<Range<usize>>,
    mmio32: Option<Range<usize>>,
    intx: [[u32; 4]; 4],
    /// (RID 基址, IOMMU phandle, stream 基址, 长度)。
    stream_map: Vec<(u32, u32, u32, u32)>,
    stream_mask: Option<u32>,
    message_map: Vec<(u32, u32, u32, u32)>,
    message_mask: Option<u32>,
    message_parent: Option<u32>,
}

#[derive(Default)]
struct IommuScan {
    phandle: Option<u32>,
    kind: Option<u32>,
    reg: Option<Range<usize>>,
}

#[derive(Default)]
struct MessageControllerScan {
    kind: Option<MessageControllerKind>,
    controller: bool,
    cells: Option<u32>,
    phandle: Option<u32>,
    supervisor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageControllerKind {
    Native,
    GicV2m,
}

/// 把同一目标控制器的 phandle 映射归一；不复制控制器状态或假设 stream/message ID 相等。
fn requester_map(
    rows: Vec<(u32, u32, u32, u32)>,
    target: Option<u32>,
    mask: Option<u32>,
) -> RequesterMap {
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(rows.len())
        .expect("PCI requester map allocation");
    for (base, phandle, target_base, length) in rows {
        assert_eq!(
            Some(phandle),
            target,
            "PCI map references an unknown controller"
        );
        ranges.push((base, target_base, length));
    }
    RequesterMap::new(ranges, mask.unwrap_or(u32::MAX))
}

/// 标准 map 条目为 RID、phandle、目标 ID、length，不能接受尾部截断。
fn decode_map(bytes: &[u8], rows: &mut Vec<(u32, u32, u32, u32)>) {
    assert!(
        !bytes.is_empty() && bytes.len().is_multiple_of(16),
        "invalid PCI requester map"
    );
    rows.try_reserve_exact(bytes.len() / 16)
        .expect("PCI requester map allocation");
    for row in bytes.as_chunks::<16>().0 {
        rows.push((
            be_u32(&row[0..4]),
            be_u32(&row[4..8]),
            be_u32(&row[8..12]),
            be_u32(&row[12..16]),
        ));
    }
}

/// 解码设备事实。
///
/// # Panics
///
/// 设备窗口、中断或 IOMMU 引用非法时 fail-stop；当前只接受一个平台 IOMMU。
pub(crate) fn parse(dtb: Dtb<'_>) -> DeviceFacts {
    let mut virtio = VirtioMmioScan::new();
    let mut pci = PciScan::default();
    let mut pci_found: Option<PciScan> = None;
    let mut iommu = IommuScan::default();
    let mut iommu_found: Option<IommuFacts> = None;
    let mut messages = MessageControllerScan::default();
    let mut message_controller = None;
    let mut commit =
        |pci: &mut PciScan, iommu: &mut IommuScan, messages: &mut MessageControllerScan| {
            let pending = core::mem::take(messages);
            if let Some(kind) = pending.kind
                && (kind == MessageControllerKind::GicV2m || MESSAGE_CELLS != 0 || pending.supervisor)
            {
                let cells_match = match kind {
                    MessageControllerKind::Native if MESSAGE_CELLS != 0 => {
                        pending.cells == Some(MESSAGE_CELLS)
                    }
                    _ => pending.cells.is_none_or(|cells| cells == 0),
                };
                assert!(
                    pending.controller && cells_match,
                    "invalid MSI controller binding"
                );
                let phandle = pending.phandle.expect("MSI controller lacks phandle");
                assert!(
                    message_controller.replace((phandle, kind)).is_none(),
                    "multiple MSI controllers are not supported"
                );
            }
            let pending = core::mem::take(pci);
            if pending.is_host && pci_found.is_none() {
                pci_found = Some(pending);
            }
            let pending = core::mem::take(iommu);
            if let Some(kind) = pending.kind {
                let reg = pending.reg.expect("IOMMU node lacks reg");
                assert!(
                    iommu_found.is_none(),
                    "multiple platform IOMMUs are not supported"
                );
                iommu_found = Some(IommuFacts {
                    kind,
                    base: reg.start,
                    size: reg.end - reg.start,
                    phandle: pending.phandle.expect("IOMMU lacks phandle"),
                });
            }
        };
    dtb.walk(|_, object| match object {
        DtbObj::SubNode { .. } => {
            virtio.begin_node();
            commit(&mut pci, &mut iommu, &mut messages);
            WalkOperation::StepInto
        }
        DtbObj::Property(Property::Reg(mut registers)) => {
            let first = registers.next();
            virtio.reg(first.clone());
            pci.ecam = pci.ecam.take().or_else(|| first.clone());
            iommu.reg = iommu.reg.take().or(first);
            WalkOperation::StepOver
        }
        DtbObj::Property(Property::Compatible(compatible)) => {
            virtio.compatible(compatible.clone());
            pci.is_host |= compatible
                .clone()
                .any(|value| value == Str::from("pci-host-ecam-generic"));
            messages.kind = compatible.clone().find_map(|value| {
                if value == Str::from(MESSAGE_COMPATIBLE) {
                    Some(MessageControllerKind::Native)
                } else if cfg!(target_arch = "aarch64") && value == Str::from(GIC_V2M_COMPATIBLE) {
                    Some(MessageControllerKind::GicV2m)
                } else {
                    None
                }
            });
            iommu.kind = iommu.kind.or_else(|| iommu_kind(compatible));
            WalkOperation::StepOver
        }
        DtbObj::Property(Property::PHandle(phandle)) => {
            iommu.phandle = Some(phandle.value());
            messages.phandle = Some(phandle.value());
            WalkOperation::StepOver
        }
        DtbObj::Property(Property::General { name, value }) => {
            if name == Str::from("interrupts") {
                virtio.interrupt(decode_interrupt(value));
            } else if name == Str::from("ranges") {
                pci.mmio32 = pci.mmio32.take().or_else(|| decode_pci_mmio32(value));
            } else if name == Str::from("interrupt-map") {
                decode_pci_interrupt_map(value, &mut pci.intx);
            } else if name == Str::from("iommu-map-mask") {
                assert_eq!(value.len(), 4, "invalid iommu-map-mask");
                pci.stream_mask = Some(be_u32(value));
            } else if name == Str::from("iommu-map") {
                decode_map(value, &mut pci.stream_map);
            } else if name == Str::from("msi-map-mask") {
                assert_eq!(value.len(), 4, "invalid msi-map-mask");
                pci.message_mask = Some(be_u32(value));
            } else if name == Str::from("msi-map") {
                decode_map(value, &mut pci.message_map);
            } else if name == Str::from("msi-parent") {
                assert_eq!(value.len(), 4, "direct MSI parent has no argument cells");
                pci.message_parent = Some(be_u32(value));
            } else if name == Str::from("interrupts-extended") {
                messages.supervisor = value
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .any(|pair| be_u32(&pair[4..]) == 9);
            } else if name == Str::from("msi-controller") {
                assert!(value.is_empty(), "invalid msi-controller marker");
                messages.controller = true;
            } else if name == Str::from("#msi-cells") {
                assert_eq!(value.len(), 4, "invalid #msi-cells");
                messages.cells = Some(be_u32(value));
            }
            WalkOperation::StepOver
        }
        DtbObj::Property(_) => WalkOperation::StepOver,
    });
    commit(&mut pci, &mut iommu, &mut messages);
    let pci = pci_found.and_then(|scan| {
        let stream = requester_map(
            scan.stream_map,
            iommu_found.map(|iommu| iommu.phandle),
            scan.stream_mask,
        );
        let messages = if let Some(parent) = scan.message_parent {
            assert!(
                MESSAGE_CELLS == 0 && scan.message_map.is_empty() && scan.message_mask.is_none(),
                "invalid direct MSI parent or overlapping msi-map"
            );
            assert_eq!(
                message_controller,
                Some((parent, MessageControllerKind::Native)),
                "PCI MSI parent must be the supervisor controller"
            );
            MessageRouting::Direct
        } else if scan.message_map.is_empty() {
            MessageRouting::None
        } else {
            let mapping = requester_map(
                scan.message_map,
                message_controller.map(|(phandle, _)| phandle),
                scan.message_mask,
            );
            // Valid firmware can describe a GICv2m frame even though the kernel
            // currently allocates ARM message interrupts only through an ITS.
            match message_controller {
                Some((_, MessageControllerKind::Native)) => MessageRouting::Remapped(mapping),
                _ => MessageRouting::None,
            }
        };
        Some(PciHost::new(
            scan.ecam.filter(|range| range.end > range.start)?,
            scan.mmio32?,
            scan.intx,
            stream,
            messages,
        ))
    });
    DeviceFacts {
        virtio: virtio.finish(),
        pci,
        iommu: iommu_found,
    }
}
