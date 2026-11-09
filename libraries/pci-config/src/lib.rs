#![no_std]
//! PCI 标准配置区的瞬时快照与 capability codec；不枚举总线，不分配 BAR/IRQ，也不访问具体 MMIO。

/// 256-byte 标准 PCI 配置区；扩展 capability 不是此链的一部分。
const CONFIGURATION_BYTES: usize = 256;

/// PCI MSI-X 表项的标准 dword 布局；设备初始化与原生中断源控制共用，不承载可变状态。
pub mod msix_table {
    /// 一个表项的完整字节长度。
    pub const ENTRY_BYTES: usize = 16;
    /// 消息地址低 32 位。
    pub const ADDRESS_LOW: usize = 0;
    /// 消息地址高 32 位。
    pub const ADDRESS_HIGH: usize = 4;
    /// 消息数据。
    pub const DATA: usize = 8;
    /// 向量控制字；只允许修改 MASK 位，其余位保留。
    pub const CONTROL: usize = 12;
    /// 向量控制字中的源屏蔽位。
    pub const MASK: u32 = 1;
}
/// 不完整、循环或越界的设备配置；调用方不能继续使用部分解码结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidConfiguration;

/// 只在一次发现事务中使用的配置快照；不是可同步写回的寄存器缓存。
pub struct Configuration([u8; CONFIGURATION_BYTES]);
impl Configuration {
    /// 从原始标准配置区建立快照，用于固件输入或 host fixture。
    const fn from_bytes(bytes: [u8; CONFIGURATION_BYTES]) -> Self {
        Self(bytes)
    }
    /// 通过已授权的配置空间读取器取得一次快照；每个 dword 只读一次，不分配。
    /// # Errors
    /// 任一底层读取失败则返回原错误，不能使用不完整快照。
    pub fn read<E>(mut read: impl FnMut(usize) -> Result<u32, E>) -> Result<Self, E> {
        let mut bytes = [0; CONFIGURATION_BYTES];
        for (index, word) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *word = read(index * 4)?.to_le_bytes();
        }
        Ok(Self::from_bytes(bytes))
    }
    /// 完整验证标准 capability 链后才交出视图；没有链时返回空视图。
    /// # Errors
    /// 非 type 0/1 header、非法指针、重复节点/循环返回 InvalidConfiguration。
    pub fn capabilities(&self) -> Result<Capabilities<'_>, InvalidConfiguration> {
        let header = self.0[0x0e] & 0x7f;
        if header > 1 {
            return Err(InvalidConfiguration);
        }
        let first = if self.0[6] & 0x10 != 0 {
            self.0[0x34] & !3
        } else {
            0
        };
        let mut next = first;
        let mut visited = 0u64;
        while next != 0 {
            if next < 0x40 {
                return Err(InvalidConfiguration);
            }
            let bit = 1u64 << (next / 4);
            if visited & bit != 0 {
                return Err(InvalidConfiguration);
            }
            visited |= bit;
            next = self.0[usize::from(next) + 1] & !3;
        }
        Ok(Capabilities {
            bytes: &self.0,
            next: first,
        })
    }
}

/// 已验证无循环的标准链；每个视图仍须校验具体 capability 的长度。
pub struct Capabilities<'a> {
    bytes: &'a [u8; CONFIGURATION_BYTES],
    next: u8,
}
impl<'a> Iterator for Capabilities<'a> {
    type Item = Capability<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next == 0 {
            return None;
        }
        let offset = self.next;
        self.next = self.bytes[usize::from(offset) + 1] & !3;
        Some(Capability {
            bytes: self.bytes,
            offset,
        })
    }
}

/// 一条 capability 的有界配置视图；不允许把设备给出的长度延伸到扩展配置区。
#[derive(Clone, Copy)]
pub struct Capability<'a> {
    bytes: &'a [u8; CONFIGURATION_BYTES],
    offset: u8,
}
impl<'a> Capability<'a> {
    /// 标准 PCI capability ID。
    pub fn id(self) -> u8 {
        self.bytes[usize::from(self.offset)]
    }
    /// 相对 function 配置区的偏移。
    fn offset(self) -> usize {
        usize::from(self.offset)
    }
    /// 借出本条目开头的 length 字节；必须至少包含两字节链头。
    /// # Errors
    /// 长度小于头部或超出标准配置区返回 InvalidConfiguration。
    pub fn bytes(self, length: usize) -> Result<&'a [u8], InvalidConfiguration> {
        if length < 2 {
            return Err(InvalidConfiguration);
        }
        let end = self
            .offset()
            .checked_add(length)
            .ok_or(InvalidConfiguration)?;
        self.bytes
            .get(self.offset()..end)
            .ok_or(InvalidConfiguration)
    }
    /// MSI/MSI-X 语义视图；其他 ID 返回 None，不猜测其厂商格式。
    /// # Errors
    /// MSI 地址/掩码字段不完整或 MSI-X 条目越界返回 InvalidConfiguration。
    pub fn message_interrupt(self) -> Result<Option<MessageInterrupt>, InvalidConfiguration> {
        if !matches!(self.id(), 5 | 0x11) {
            return Ok(None);
        }
        let prefix = self.bytes(4)?;
        let control = u16::from_le_bytes([prefix[2], prefix[3]]);
        let msix = if self.id() == 5 {
            let bytes = match (control & 0x80 != 0, control & 0x100 != 0) {
                (false, false) => 10,
                (true, false) => 14,
                (false, true) => 20,
                (true, true) => 24,
            };
            self.bytes(bytes)?;
            None
        } else {
            let bytes = self.bytes(12)?;
            let entries = (control & 0x7ff) + 1;
            let table =
                u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| InvalidConfiguration)?);
            let pending =
                u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| InvalidConfiguration)?);
            Some(Msix {
                table: BarRegion {
                    bar: (table & 7) as u8,
                    offset: table & !7,
                    length: u32::from(entries) * msix_table::ENTRY_BYTES as u32,
                },
                pending: BarRegion {
                    bar: (pending & 7) as u8,
                    offset: pending & !7,
                    length: u32::from(entries).div_ceil(64) * 8,
                },
                entries,
            })
        };
        Ok(Some(MessageInterrupt {
            offset: self.offset(),
            control,
            msix,
        }))
    }
}

/// 原始消息中断控制；MSI-X 的表位置由同一条目派生，不复制可变中断状态。
pub struct MessageInterrupt {
    offset: usize,
    control: u16,
    msix: Option<Msix>,
}
impl MessageInterrupt {
    /// 相对 function 配置区的消息控制寄存器偏移；写入时必须读取当前值，不能用发现快照覆盖运行状态。
    pub fn control_offset(&self) -> usize {
        self.offset + 2
    }
    /// 返回控制寄存器偏移和禁止消息投递的值；只用于冷发现时撤下固件遗留配置。
    /// 调用方在独占配置事务中写回，不得长期保存此快照并覆盖驱动的新配置。
    pub fn disabled_control(&self) -> (usize, u16) {
        (
            self.offset + 2,
            if self.msix.is_some() {
                self.control & !0x8000 | 0x4000
            } else {
                self.control & !1
            },
        )
    }
    /// MSI-X 的表/PBA 描述；普通 MSI 为 None。
    pub fn msix(&self) -> Option<&Msix> {
        self.msix.as_ref()
    }
}

/// 一个 MSI-X 区间相对 BAR 的投影；不能将 64-bit BAR 的高半字或不存在 BAR 当作目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarRegion {
    bar: u8,
    offset: u32,
    length: u32,
}
impl BarRegion {
    fn end(self) -> u64 {
        u64::from(self.offset) + u64::from(self.length)
    }
    fn valid(self, size: Option<u64>) -> bool {
        self.bar < 6 && size.is_some_and(|size| self.end() <= size)
    }
}
/// 标准 MSI-X 表与 pending bit array；启用、mask 与 IRQ 寿命由设备/平台拥有。
pub struct Msix {
    table: BarRegion,
    pending: BarRegion,
    entries: u16,
}
impl Msix {
    /// 硬件表的槽数（1..=2048）。
    pub fn entries(&self) -> u16 {
        self.entries
    }
    /// 表的 BAR 编号、BAR 内字节偏移与完整字节长度；映射前仍须 validate。
    pub fn table(&self) -> (u8, u32, u32) {
        (self.table.bar, self.table.offset, self.table.length)
    }
    /// 验证完整表与 PBA 均在实际 memory BAR 内，且两者不重叠。
    /// # Errors
    /// 无效 BAR、区间越界或重叠返回 InvalidConfiguration。
    pub fn validate(
        &self,
        mut bar_size: impl FnMut(u8) -> Option<u64>,
    ) -> Result<(), InvalidConfiguration> {
        if !(1..=2048).contains(&self.entries)
            || self.table.length != u32::from(self.entries) * msix_table::ENTRY_BYTES as u32
            || self.pending.length != u32::from(self.entries).div_ceil(64) * 8
            || !self.table.valid(bar_size(self.table.bar))
            || !self.pending.valid(bar_size(self.pending.bar))
            || self.table.bar == self.pending.bar
                && u64::from(self.table.offset) < self.pending.end()
                && u64::from(self.pending.offset) < self.table.end()
        {
            return Err(InvalidConfiguration);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_chain_and_msix_regions_never_escape_the_snapshot_or_bar() {
        let mut bytes = [0u8; 256];
        bytes[6] = 0x10;
        bytes[0x34] = 0x40;
        bytes[0x40..0x4c].copy_from_slice(&[0x11, 0, 3, 0xc0, 0, 1, 0, 0, 0, 2, 0, 0]);
        let snapshot = Configuration::from_bytes(bytes);
        let message = snapshot
            .capabilities()
            .unwrap()
            .next()
            .unwrap()
            .message_interrupt()
            .unwrap()
            .unwrap();
        assert_eq!(message.disabled_control(), (0x42, 0x4003));
        let msix = message.msix().unwrap();
        assert_eq!(msix.entries, 4);
        assert_eq!(
            msix.table,
            BarRegion {
                bar: 0,
                offset: 256,
                length: 64
            }
        );
        msix.validate(|bar| (bar == 0).then_some(520)).unwrap();
        assert!(msix.validate(|_| Some(519)).is_err());
        let mut overlap = bytes;
        overlap[0x48..0x4c].copy_from_slice(&256u32.to_le_bytes());
        let overlap = Configuration::from_bytes(overlap);
        let message = overlap
            .capabilities()
            .unwrap()
            .next()
            .unwrap()
            .message_interrupt()
            .unwrap()
            .unwrap();
        assert!(message.msix().unwrap().validate(|_| Some(4096)).is_err());
        let mut large = bytes;
        large[0x42..0x44].copy_from_slice(&64u16.to_le_bytes()); // 65 vectors，PBA 跨两个 64-bit word。
        large[0x48..0x4c].copy_from_slice(&2048u32.to_le_bytes());
        let large = Configuration::from_bytes(large);
        let message = large
            .capabilities()
            .unwrap()
            .next()
            .unwrap()
            .message_interrupt()
            .unwrap()
            .unwrap();
        message.msix().unwrap().validate(|_| Some(2064)).unwrap();
        assert!(message.msix().unwrap().validate(|_| Some(2063)).is_err());
        bytes[0x41] = 0x40;
        assert!(Configuration::from_bytes(bytes).capabilities().is_err());
        bytes[0x41] = 0x3c;
        assert!(Configuration::from_bytes(bytes).capabilities().is_err());
        bytes[0x34] = 0xfc;
        bytes[0xfc] = 0x11;
        bytes[0xfd] = 0;
        let snapshot = Configuration::from_bytes(bytes);
        assert!(
            snapshot
                .capabilities()
                .unwrap()
                .next()
                .unwrap()
                .message_interrupt()
                .is_err()
        );
        let mut bytes = [0u8; 256];
        bytes[6] = 0x10;
        bytes[0x34] = 0x40;
        bytes[0x40] = 5;
        bytes[0x42..0x44].copy_from_slice(&0x181u16.to_le_bytes());
        let snapshot = Configuration::from_bytes(bytes);
        let message = snapshot
            .capabilities()
            .unwrap()
            .next()
            .unwrap()
            .message_interrupt()
            .unwrap()
            .unwrap();
        assert_eq!(message.disabled_control(), (0x42, 0x180));
        assert!(message.msix().is_none());
        bytes[0x34] = 0xf0;
        bytes[0xf0] = 5;
        bytes[0xf2..0xf4].copy_from_slice(&0x101u16.to_le_bytes());
        let snapshot = Configuration::from_bytes(bytes);
        assert!(
            snapshot
                .capabilities()
                .unwrap()
                .next()
                .unwrap()
                .message_interrupt()
                .is_err()
        );
    }
}
