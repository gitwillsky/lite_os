//! PCI ECAM 根总线的唯一枚举与 BAR 分配 owner；设备协议由各 adapter 解码。
use crate::{arch::mmio::MmioBus, platform::PciHost};
use alloc::vec::Vec;
use spin::Once;

const BAR0: usize = 0x10;
const COMMAND: usize = 0x04;
const PCI_FUNCTIONS_PER_SLOT: usize = 8;

/// 已分配且互不重叠的 memory BAR。
#[derive(Clone, Copy)]
pub(crate) struct Bar {
    pub(crate) address: usize,
    pub(crate) size: usize,
}
/// 一份根总线 function；配置与 BAR 只在初始化阶段写入，随后只允许所属 adapter 使用。
pub(crate) struct Function {
    pub(crate) config: MmioBus,
    pub(crate) bars: [Option<Bar>; 6],
    pub(crate) interrupt: Option<u32>,
}
// OWNER: 初始化后不可变的 PCI inventory；唯一 BAR 分配事务避免不同 adapter 重叠分配。
// PROOF: main 在总线绑定前串行初始化，Once 发布后只借用；失败时启动 fail-stop，不运行半初始化设备。
// FAILURE: 各 adapter 自行从窗口起点分配会让 VirtIO 与后续 USB host 覆盖同一 BAR。
static FUNCTIONS: Once<Vec<Function>> = Once::new();

/// 枚举平台 ECAM 根总线并分配 memory BAR；无 PCI host 时发布空表。
/// 分配/配置失败返回错误，不能发布有重叠或不完整 BAR 的设备。
pub(crate) fn initialize(host: Option<&PciHost>) -> Result<(), ()> {
    if FUNCTIONS.get().is_some() {
        return Err(());
    }
    let mut functions = Vec::new();
    if let Some(host) = host {
        let mut allocation = host.mmio32.start;
        for slot in 0..32 {
            for function in 0..PCI_FUNCTIONS_PER_SLOT {
                let offset = (slot << 15) | (function << 12);
                if offset + 4096 > host.ecam.len() {
                    break;
                }
                let config = MmioBus::new(
                    crate::platform::map_device_window(host.ecam.start + offset),
                    4096,
                )
                .map_err(|_| ())?;
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
                let bars =
                    assign_memory_bars(&config, &mut allocation, host.mmio32.end).ok_or(())?;
                config.write_u16(COMMAND, command | 6).map_err(|_| ())?;
                let pin = config.read_u8(0x3d).map_err(|_| ())? as usize;
                functions.try_reserve(1).map_err(|_| ())?;
                functions.push(Function {
                    config,
                    bars,
                    interrupt: host.interrupt(slot, pin),
                });
                if function == 0 && header & 0x80 == 0 {
                    break;
                }
            }
        }
    }
    FUNCTIONS.call_once(|| functions);
    Ok(())
}
/// 按发现顺序借用 PCI functions；必须在 initialize 成功之后调用。
pub(crate) fn functions() -> &'static [Function] {
    FUNCTIONS.get().expect("PCI inventory not initialized")
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
            address: *allocation,
            size,
        });
        *allocation = end;
        index += if is_64 { 2 } else { 1 };
    }
    Some(bars)
}
