//! @description QEMU `virt` generic ECAM host and the UTM VirtIO Console function.

use super::discovery::PciHostInfo;
use crate::drivers::{MmioBus, PciTransport};

const CONFIG_BYTES: usize = 4096;
const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const BAR0: usize = 0x10;
const CAPABILITIES: usize = 0x34;
const INTERRUPT_PIN: usize = 0x3d;
const SUBSYSTEM_DEVICE_ID: usize = 0x2e;
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_CAP_ID_VENDOR: u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

#[derive(Clone, Copy)]
struct Bar {
    address: usize,
    size: usize,
}

#[derive(Clone, Copy)]
struct Capability {
    bar: usize,
    offset: usize,
    length: usize,
}

/// @description One discovered modern VirtIO PCI function and its routed INTx vector.
pub(crate) struct VirtioPciFunction {
    pub(crate) device_id: u32,
    pub(crate) interrupt: u32,
    pub(crate) transport: PciTransport,
}

pub(crate) fn find_console(host: PciHostInfo) -> Option<VirtioPciFunction> {
    let mut allocation = host.mmio32.start;
    let allocation_end = host.mmio32.start.checked_add(host.mmio32.size)?;
    for slot in 0..32 {
        let config = function_config(host, 0, slot, 0)?;
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

        let bars = assign_memory_bars(&config, &mut allocation, allocation_end)?;
        let transport = capabilities(&config, &bars)?;
        let pin = usize::from(config.read_u8(INTERRUPT_PIN).ok()?);
        let interrupt = host.interrupt(slot, pin)?;
        let command = config.read_u16(COMMAND).ok()?;
        config.write_u16(COMMAND, command | 0x0006).ok()?;
        return Some(VirtioPciFunction {
            device_id,
            interrupt,
            transport,
        });
    }
    None
}

fn function_config(host: PciHostInfo, bus: usize, slot: usize, function: usize) -> Option<MmioBus> {
    if bus >= 16 || slot >= 32 || function >= 8 {
        return None;
    }
    let offset = (bus << 20) | (slot << 15) | (function << 12);
    let physical = host.ecam.start.checked_add(offset)?;
    MmioBus::new(
        crate::arch::mmu::physical_to_virtual(physical),
        CONFIG_BYTES,
    )
    .ok()
}

fn assign_memory_bars(
    config: &MmioBus,
    allocation: &mut usize,
    allocation_end: usize,
) -> Option<[Option<Bar>; 6]> {
    let mut bars = [None; 6];
    let mut index = 0usize;
    while index < bars.len() {
        let offset = BAR0 + index * 4;
        let original = config.read_u32(offset).ok()?;
        config.write_u32(offset, u32::MAX).ok()?;
        let mask = config.read_u32(offset).ok()?;
        config.write_u32(offset, original).ok()?;
        if mask == 0 || mask == u32::MAX || mask & 1 != 0 {
            index += 1;
            continue;
        }
        let is_64 = mask & 0x6 == 0x4;
        let low_mask = u64::from(mask & !0xf);
        let full_mask = if is_64 {
            let high_original = config.read_u32(offset + 4).ok()?;
            config.write_u32(offset + 4, u32::MAX).ok()?;
            let high_mask = config.read_u32(offset + 4).ok()?;
            config.write_u32(offset + 4, high_original).ok()?;
            u64::from(high_mask) << 32 | low_mask
        } else {
            0xffff_ffff_0000_0000 | low_mask
        };
        let size = usize::try_from((!full_mask).checked_add(1)?).ok()?;
        if size == 0 || !size.is_power_of_two() {
            return None;
        }
        *allocation = allocation.checked_add(size - 1)? & !(size - 1);
        let end = allocation.checked_add(size)?;
        if end > allocation_end || *allocation > u32::MAX as usize {
            return None;
        }
        config
            .write_u32(offset, (*allocation as u32) | (original & 0xf))
            .ok()?;
        if is_64 {
            config.write_u32(offset + 4, 0).ok()?;
        }
        bars[index] = Some(Bar {
            address: *allocation,
            size,
        });
        *allocation = end;
        index += if is_64 { 2 } else { 1 };
    }
    Some(bars)
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
        crate::arch::mmu::physical_to_virtual(physical),
        capability.length,
    )
    .ok()
}
