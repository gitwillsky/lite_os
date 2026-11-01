//! 从平台 DTB 提取 devmgr 驱动所需的设备事实。
//!
//! 只关心由用户态驱动拥有的设备：VirtIO-MMIO transport、通用 PCI ECAM host 与 IOMMU。
//! 中断控制器、串口、RTC 与定时器由内核拥有，这里不解析。中断说明符按架构解码：
//! AArch64 为 GICv3 三 cell（SPI 号 + 32），RISC-V 为 PLIC 单 cell（source 号）。

use alloc::vec::Vec;
use core::ops::Range;

use dtb_walker::{Dtb, DtbObj, Property, Str, WalkOperation};

use super::pci_host::PciHost;
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

/// devmgr 使用的设备事实。
pub(crate) struct DeviceFacts {
    pub(crate) virtio: VirtioMmioTransports,
    pub(crate) pci: Option<PciHost>,
    pub(crate) iommu: Option<IommuFacts>,
}

/// 中断说明符的 cell 数。
#[cfg(target_arch = "aarch64")]
const INTERRUPT_CELLS: usize = 3;
#[cfg(target_arch = "riscv64")]
const INTERRUPT_CELLS: usize = 1;
/// PCI `interrupt-map` 每项中父中断控制器的地址 cell 数（GICv3 为 2，PLIC 为 0）。
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
        Some(be_u32(cells)).filter(|&source| source != 0)
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
}

#[derive(Default)]
struct IommuScan {
    phandle: Option<u32>,
    kind: Option<u32>,
    reg: Option<Range<usize>>,
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
    let mut commit = |pci: &mut PciScan, iommu: &mut IommuScan| {
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
            commit(&mut pci, &mut iommu);
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
            iommu.kind = iommu.kind.or_else(|| iommu_kind(compatible));
            WalkOperation::StepOver
        }
        DtbObj::Property(Property::PHandle(phandle)) => {
            iommu.phandle = Some(phandle.value());
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
                assert!(
                    !value.is_empty() && value.len().is_multiple_of(16),
                    "invalid iommu-map"
                );
                for row in value.as_chunks::<16>().0 {
                    pci.stream_map.push((
                        be_u32(&row[0..4]),
                        be_u32(&row[4..8]),
                        be_u32(&row[8..12]),
                        be_u32(&row[12..16]),
                    ));
                }
            }
            WalkOperation::StepOver
        }
        DtbObj::Property(_) => WalkOperation::StepOver,
    });
    commit(&mut pci, &mut iommu);
    let pci = pci_found.and_then(|scan| {
        let mut stream_map = Vec::new();
        for (base, phandle, stream_base, length) in scan.stream_map {
            assert!(
                iommu_found.is_some_and(|iommu| iommu.phandle == phandle),
                "PCI map references an unknown IOMMU"
            );
            assert!(
                length != 0
                    && base.checked_add(length - 1).is_some()
                    && stream_base.checked_add(length - 1).is_some(),
                "invalid IOMMU stream range"
            );
            assert!(
                !stream_map.iter().any(|&(old_base, _, old_length)| {
                    (old_base..=old_base + old_length - 1).contains(&base)
                        || (base..=base + length - 1).contains(&old_base)
                }),
                "overlapping iommu-map ranges"
            );
            stream_map.push((base, stream_base, length));
        }
        Some(PciHost::new(
            scan.ecam.filter(|range| range.end > range.start)?,
            scan.mmio32?,
            scan.intx,
            stream_map,
            scan.stream_mask.unwrap_or(u32::MAX),
        ))
    });
    DeviceFacts {
        virtio: virtio.finish(),
        pci,
        iommu: iommu_found,
    }
}
