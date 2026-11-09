//! PCI ECAM 根总线的枚举、BAR 分配与初始中断接管 owner；function 的设备协议由宿主中的驱动解码。
use crate::platform::PciHost;
use alloc::vec::Vec;
use launch_protocol::driver_host::Bar;
use mmio::MmioBus;

const BAR0: usize = 0x10;
const COMMAND: usize = 0x04;
const PCI_FUNCTIONS_PER_SLOT: usize = 8;
/// 根总线（bus 0）在 ECAM 中占用的字节数：32 slot × 8 function × 4 KiB。
const ROOT_BUS_ECAM_BYTES: usize = 32 * PCI_FUNCTIONS_PER_SLOT * 4096;

/// 一份根总线 function：配置空间页、已分配的 BAR、INTx 路由与 DMA stream。
pub(crate) struct Function {
    /// 配置空间页的物理地址。
    pub(crate) config_address: usize,
    /// 已映射的配置空间（manager 只用于匹配驱动）。
    pub(crate) config: MmioBus,
    pub(crate) bars: [Option<Bar>; 6],
    pub(crate) interrupt: Option<u32>,
    /// 请求者 ID 经 `iommu-map` 换算的 stream；没有 IOMMU 时为 `None`。
    pub(crate) stream: Option<u32>,
    /// 同一 requester 经独立 msi-map 得到的消息 DeviceID。
    pub(crate) message_device: Option<u32>,
}

/// 枚举平台 ECAM 根总线并分配 memory BAR；无 PCI host 时返回空表。
///
/// # Errors
///
/// 分配/配置失败返回错误，不能发布有重叠或不完整 BAR 的设备。
pub(crate) fn enumerate(host: Option<&PciHost>) -> Result<Vec<Function>, ()> {
    let mut functions = Vec::new();
    let Some(host) = host else {
        return Ok(functions);
    };
    let mut allocation = host.mmio32.start;
    let ecam_bytes = host.ecam.len().min(ROOT_BUS_ECAM_BYTES);
    let ecam = crate::platform::map_device_window(host.ecam.start, ecam_bytes);
    for slot in 0..32 {
        for function in 0..PCI_FUNCTIONS_PER_SLOT {
            let offset = (slot << 15) | (function << 12);
            if offset + 4096 > ecam_bytes {
                break;
            }
            let config = MmioBus::new(ecam + offset, 4096).map_err(|_| ())?;
            let vendor = config.read_u16(0).map_err(|_| ())?;
            if vendor == 0xffff {
                if function == 0 {
                    break;
                } else {
                    continue;
                }
            }
            let header = config.read_u8(0x0e).map_err(|_| ())?;
            if header & 0x7f != 0 {
                return Err(());
            }
            let command = config.read_u16(COMMAND).map_err(|_| ())?;
            config.write_u16(COMMAND, command & !6).map_err(|_| ())?;
            // 1. 独占枚举时先撤下固件遗留 MSI/MSI-X，再移动 BAR；否则设备仍可向旧目标发消息。
            let snapshot = pci_config::Configuration::read(|offset| config.read_u32(offset))
                .map_err(|_| ())?;
            for capability in snapshot.capabilities().map_err(|_| ())? {
                if let Some(message) = capability.message_interrupt().map_err(|_| ())? {
                    let (offset, disabled) = message.disabled_control();
                    config.write_u16(offset, disabled).map_err(|_| ())?;
                }
            }
            let bars = assign_memory_bars(&config, &mut allocation, host.mmio32.end).ok_or(())?;
            for capability in snapshot.capabilities().map_err(|_| ())? {
                if let Some(message) = capability.message_interrupt().map_err(|_| ())?
                    && let Some(msix) = message.msix()
                {
                    msix.validate(|bar| {
                        bars.get(usize::from(bar))
                            .copied()
                            .flatten()
                            .map(|bar| bar.size)
                    })
                    .map_err(|_| ())?;
                }
            }
            // 2. 当前宿主按 INTx 接管；不能保留固件的 INTx-disable 位而静默丢失所有通知。
            config
                .write_u16(COMMAND, (command | 6) & !(1 << 10))
                .map_err(|_| ())?;
            let pin = config.read_u8(0x3d).map_err(|_| ())? as usize;
            functions.try_reserve(1).map_err(|_| ())?;
            functions.push(Function {
                config_address: host.ecam.start + offset,
                config,
                bars,
                interrupt: host.interrupt(slot, pin),
                // 根总线 requester ID = slot << 3 | function（PCIe BDF，bus 0）。
                stream: host.stream(((slot << 3) | function) as u32),
                message_device: host.message_device(((slot << 3) | function) as u32),
            });
            if function == 0 && header & 0x80 == 0 {
                break;
            }
        }
    }
    Ok(functions)
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
        if is_64 && index + 1 == bars.len() {
            return None;
        }
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
            address: *allocation as u64,
            size: size as u64,
        });
        *allocation = end;
        index += if is_64 { 2 } else { 1 };
    }
    Some(bars)
}
