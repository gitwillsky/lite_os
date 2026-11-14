//! VirtIO PCI vendor-capability decoding for this host's node; PCI resources belong to drivers::pci.

use super::PciTransport;
use crate::drivers::pci::Bar;
use mmio::MmioBus;

const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
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
    pub(super) interrupt: Option<u32>,
    pub(super) transport: PciTransport,
    pub(super) bti: &'static native_runtime::Bti,
}

/// 解码本节点的 VirtIO PCI function：校验厂商与 capability，返回 transport。
///
/// # Returns
///
/// `message_interrupts` 由实际 adapter 的绑定路径声明支持；未支持的 adapter 保持 INTx，不能因硬件有表就启用无人消费的消息。
/// 缺 capability、非法消息表或没有实际可用中断路由时为 `None`。
pub(super) fn discover(message_interrupts: bool) -> Option<VirtioPciFunction> {
    let function = crate::drivers::pci::function()?;
    let config = &function.config;
    if config.read_u16(VENDOR_ID).ok()? != PCI_VENDOR_VIRTIO {
        return None;
    }
    let pci_device = config.read_u16(DEVICE_ID).ok()?;
    let device_id = if (0x1040..=0x107f).contains(&pci_device) {
        u32::from(pci_device - 0x1040)
    } else if (0x1000..=0x103f).contains(&pci_device) {
        u32::from(config.read_u16(SUBSYSTEM_DEVICE_ID).ok()?)
    } else {
        return None;
    };
    let transport = capabilities(
        config,
        &function.bars,
        message_interrupts && crate::platform::node().has_msi(),
    )?;
    if function.interrupt.is_none() && transport.message_interrupts().is_none() {
        return None;
    }
    Some(VirtioPciFunction {
        device_id,
        interrupt: function.interrupt,
        transport,
        bti: function.bti,
    })
}

fn capabilities(config: &MmioBus, bars: &[Option<Bar>; 6], messages: bool) -> Option<PciTransport> {
    let snapshot = pci_config::Configuration::read(|offset| config.read_u32(offset)).ok()?;
    let mut msix_owner = None;
    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = None;
    let mut isr = None;
    let mut device = None;
    for entry in snapshot.capabilities().ok()? {
        if let Some(message) = entry.message_interrupt().ok()?
            && let Some(msix) = message.msix()
        {
            msix.validate(|bar| {
                bars.get(usize::from(bar))
                    .copied()
                    .flatten()
                    .map(|bar| bar.size as u64)
            })
            .ok()?;
            if messages {
                let (bar, offset, length) = msix.table();
                let physical = capability_physical(
                    bars,
                    Capability {
                        bar: usize::from(bar),
                        offset: offset as usize,
                        length: length as usize,
                    },
                )?;
                let (address, table_vmo, table_offset) =
                    crate::platform::map_device_vmo(physical, length as usize).ok()?;
                let table = MmioBus::new(address, length as usize).ok()?;
                if msix_owner
                    .replace(
                        super::pci::Msix::new(
                            *config,
                            message.control_offset(),
                            table,
                            table_vmo,
                            table_offset,
                            msix.entries(),
                        )
                        .ok()?,
                    )
                    .is_some()
                {
                    return None;
                }
            }
        }
        if entry.id() != PCI_CAP_ID_VENDOR {
            continue;
        }
        let length = usize::from(*entry.bytes(3).ok()?.get(2)?);
        let bytes = entry.bytes(length).ok()?;
        if length < 4 {
            continue;
        }
        let kind = bytes[3];
        if !matches!(
            kind,
            VIRTIO_PCI_CAP_COMMON_CFG
                | VIRTIO_PCI_CAP_NOTIFY_CFG
                | VIRTIO_PCI_CAP_ISR_CFG
                | VIRTIO_PCI_CAP_DEVICE_CFG
        ) {
            continue;
        }
        if length < 16 {
            return None;
        }
        let capability = Capability {
            bar: usize::from(bytes[4]),
            offset: usize::try_from(u32::from_le_bytes(bytes[8..12].try_into().ok()?)).ok()?,
            length: usize::try_from(u32::from_le_bytes(bytes[12..16].try_into().ok()?)).ok()?,
        };
        match kind {
            VIRTIO_PCI_CAP_COMMON_CFG if common.is_none() => common = Some(capability),
            VIRTIO_PCI_CAP_NOTIFY_CFG if notify.is_none() && length >= 20 => {
                notify = Some(capability);
                notify_multiplier = Some(u32::from_le_bytes(bytes[16..20].try_into().ok()?));
            }
            VIRTIO_PCI_CAP_ISR_CFG if isr.is_none() => isr = Some(capability),
            VIRTIO_PCI_CAP_DEVICE_CFG if device.is_none() => device = Some(capability),
            _ => {}
        }
    }
    Some(PciTransport::new(
        capability_bus(bars, common?)?,
        capability_bus(bars, notify?)?,
        capability_bus(bars, isr?)?,
        device.and_then(|value| capability_bus(bars, value)),
        notify_multiplier?,
        msix_owner,
    ))
}

fn capability_bus(bars: &[Option<Bar>; 6], capability: Capability) -> Option<MmioBus> {
    let physical = capability_physical(bars, capability)?;
    MmioBus::new(
        crate::platform::map_device_window(physical, capability.length),
        capability.length,
    )
    .ok()
}

fn capability_physical(bars: &[Option<Bar>; 6], capability: Capability) -> Option<usize> {
    let bar = bars.get(capability.bar).copied().flatten()?;
    let end = capability.offset.checked_add(capability.length)?;
    if capability.length == 0 || end > bar.size {
        return None;
    }
    bar.address.checked_add(capability.offset)
}
