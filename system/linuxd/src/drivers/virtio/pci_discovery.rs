//! VirtIO PCI function selection and vendor-capability decoding; PCI resources belong to drivers::pci.

use super::PciTransport;
use crate::{arch::mmio::MmioBus, drivers::pci::Bar};

const CONFIG_BYTES: usize = 4096;
const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const STATUS: usize = 0x06;
const CAPABILITIES: usize = 0x34;
const SUBSYSTEM_DEVICE_ID: usize = 0x2e;
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_CAP_ID_VENDOR: u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

#[derive(Clone, Copy)]
struct Capability {
    bar: usize,
    offset: usize,
    length: usize,
}

/// One discovered modern VirtIO PCI function and its routed INTx vector.
pub(super) struct VirtioPciFunction {
    pub(super) device_id: u32,
    pub(super) interrupt: u32,
    pub(super) transport: PciTransport,
}

pub(super) fn find_console() -> Option<VirtioPciFunction> {
    for function in crate::drivers::pci::functions() {
        let config = &function.config;
        if config.read_u16(VENDOR_ID).ok()? != PCI_VENDOR_VIRTIO {
            continue;
        }
        let pci_device = config.read_u16(DEVICE_ID).ok()?;
        let device_id = if (0x1040..=0x107f).contains(&pci_device) {
            u32::from(pci_device - 0x1040)
        } else if (0x1000..=0x103f).contains(&pci_device) {
            u32::from(config.read_u16(SUBSYSTEM_DEVICE_ID).ok()?)
        } else {
            continue;
        };
        if device_id != 3 {
            continue;
        }
        return Some(VirtioPciFunction {
            device_id,
            interrupt: function.interrupt?,
            transport: capabilities(config, &function.bars)?,
        });
    }
    None
}

fn capabilities(config: &MmioBus, bars: &[Option<Bar>; 6]) -> Option<PciTransport> {
    if config.read_u16(STATUS).ok()? & 0x10 == 0 {
        return None;
    }
    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = None;
    let mut isr = None;
    let mut device = None;
    let mut current = usize::from(config.read_u8(CAPABILITIES).ok()? & !3);
    for _ in 0..48 {
        if current == 0 {
            break;
        }
        if current < 0x40 || current + 16 > CONFIG_BYTES {
            return None;
        }
        let next = usize::from(config.read_u8(current + 1).ok()? & !3);
        if config.read_u8(current).ok()? == PCI_CAP_ID_VENDOR {
            let length = usize::from(config.read_u8(current + 2).ok()?);
            let kind = config.read_u8(current + 3).ok()?;
            let capability = Capability {
                bar: usize::from(config.read_u8(current + 4).ok()?),
                offset: usize::try_from(config.read_u32(current + 8).ok()?).ok()?,
                length: usize::try_from(config.read_u32(current + 12).ok()?).ok()?,
            };
            match kind {
                VIRTIO_PCI_CAP_COMMON_CFG if common.is_none() => common = Some(capability),
                VIRTIO_PCI_CAP_NOTIFY_CFG if notify.is_none() && length >= 20 => {
                    notify = Some(capability);
                    notify_multiplier = Some(config.read_u32(current + 16).ok()?);
                }
                VIRTIO_PCI_CAP_ISR_CFG if isr.is_none() => isr = Some(capability),
                VIRTIO_PCI_CAP_DEVICE_CFG if device.is_none() => device = Some(capability),
                _ => {}
            }
        }
        current = next;
    }
    Some(PciTransport::new(
        capability_bus(bars, common?)?,
        capability_bus(bars, notify?)?,
        capability_bus(bars, isr?)?,
        device.and_then(|value| capability_bus(bars, value)),
        notify_multiplier?,
    ))
}

fn capability_bus(bars: &[Option<Bar>; 6], capability: Capability) -> Option<MmioBus> {
    let bar = bars.get(capability.bar).copied().flatten()?;
    let end = capability.offset.checked_add(capability.length)?;
    if capability.length == 0 || end > bar.size {
        return None;
    }
    let physical = bar.address.checked_add(capability.offset)?;
    MmioBus::new(
        crate::platform::map_device_window(physical),
        capability.length,
    )
    .ok()
}
