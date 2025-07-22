use core::{
    fmt::{self, Display},
    ops::Range,
};

use dtb_walker::{Dtb, DtbObj, HeaderError, Property, Str, WalkOperation};


pub struct StringInLine<const N: usize>(usize, [u8; N]);

/// VirtIO MMIO 设备信息
#[derive(Debug, Clone, Copy)]
pub struct VirtIODevice {
    pub base_addr: usize,
    pub size: usize,
    pub irq: u32,
}

pub struct BoardInfo {
    pub dtb: Range<usize>,
    pub model: StringInLine<128>,
    pub smp: usize,
    pub time_base_freq: u64,
    pub mem: Range<usize>,
    pub uart: Range<usize>,
    pub test: Range<usize>,
    pub clint: Range<usize>,
    pub virtio_devices: [Option<VirtIODevice>; 20],
    pub virtio_count: usize,
}

impl<const N: usize> Display for StringInLine<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", unsafe {
            core::str::from_utf8_unchecked(&self.1[..self.0])
        })
    }
}

impl Display for BoardInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "DTB: {:#x?}", self.dtb)?;
        writeln!(f, "Model: {}", self.model)?;
        writeln!(f, "SMP: {}", self.smp)?;
        writeln!(f, "Time Base Frequency: {}", self.time_base_freq)?;
        writeln!(f, "Memory: {:#x?}", self.mem)?;
        writeln!(f, "UART: {:#x?}", self.uart)?;
        writeln!(f, "Test: {:#x?}", self.test)?;
        writeln!(f, "CLINT: {:#x?}", self.clint)?;
        writeln!(f, "VirtIO Devices: {} found", self.virtio_count)?;
        for i in 0..self.virtio_count {
            if let Some(dev) = &self.virtio_devices[i] {
                writeln!(f, "  VirtIO[{}]: {:#x}-{:#x}, IRQ: {}", i, dev.base_addr, dev.base_addr + dev.size, dev.irq)?;
            }
        }
        Ok(())
    }
}

impl BoardInfo {
    pub fn parse(dtb_addr: usize) -> Self {
        const CPUS: &str = "cpus";
        const MEM: &str = "memory";
        const SOC: &str = "soc";
        const UART: &str = "uart";
        const SERIAL: &str = "serial";
        const TEST: &str = "test";
        const CLINT: &str = "clint";
        const VIRTIO: &str = "virtio_mmio";

        let mut ans = BoardInfo {
            dtb: dtb_addr..dtb_addr,
            model: StringInLine(0, [0; 128]),
            smp: 0,
            mem: 0..0,
            uart: 0..0,
            test: 0..0,
            clint: 0..0,
            time_base_freq: 0,
            virtio_devices: [None; 20],
            virtio_count: 0,
        };

        // 用于临时存储当前 VirtIO 设备的信息
        let mut current_virtio_reg: Option<Range<usize>> = None;
        let mut current_virtio_irq: Option<u32> = None;

        let dtb = unsafe {
            Dtb::from_raw_parts_filtered(dtb_addr as *const u8, |node| {
                matches!(
                    node,
                    HeaderError::Misaligned(4) | HeaderError::LastCompVersion(_)
                )
            })
        }
        .unwrap();

        ans.dtb.end += dtb.total_size();
        dtb.walk(|ctx, obj| match obj {
            DtbObj::SubNode { name, .. } => {
                let current = ctx.name();
                if ctx.is_root() {
                    if name == Str::from(CPUS) || name == Str::from(SOC) || name.starts_with(MEM) {
                        WalkOperation::StepInto
                    } else if name.starts_with(VIRTIO) {
                        // 遇到 VirtIO 设备节点，准备解析
                        current_virtio_reg = None;
                        current_virtio_irq = None;
                        WalkOperation::StepInto
                    } else {
                        WalkOperation::StepOver
                    }
                } else if current == Str::from(SOC) {
                    if name.starts_with(UART)
                        || name.starts_with(TEST)
                        || name.starts_with(CLINT)
                        || name.starts_with(SERIAL)
                        || name.starts_with(VIRTIO)
                    {
                        if name.starts_with(VIRTIO) {
                            // SOC 下的 VirtIO 设备
                            current_virtio_reg = None;
                            current_virtio_irq = None;
                        }
                        WalkOperation::StepInto
                    } else {
                        WalkOperation::StepOver
                    }
                } else {
                    if current == Str::from(CPUS) && name.starts_with("cpu@") {
                        ans.smp += 1;
                    }
                    WalkOperation::StepOver
                }
            }
            DtbObj::Property(Property::Model(model)) if ctx.is_root() => {
                ans.model.0 = model.as_bytes().len();
                ans.model.1[..ans.model.0].copy_from_slice(model.as_bytes());
                WalkOperation::StepOver
            }
            DtbObj::Property(Property::Reg(mut reg)) => {
                let node = ctx.name();
                if node.starts_with(UART) || node.starts_with(SERIAL) {
                    ans.uart = reg.next().unwrap();
                    WalkOperation::StepOut
                } else if node.starts_with(TEST) {
                    ans.test = reg.next().unwrap();
                    WalkOperation::StepOut
                } else if node.starts_with(CLINT) {
                    ans.clint = reg.next().unwrap();
                    WalkOperation::StepOut
                } else if node.starts_with(MEM) {
                    ans.mem = reg.next().unwrap();
                    WalkOperation::StepOut
                } else if node.starts_with(VIRTIO) {
                    // VirtIO 设备的 reg 属性
                    if let Some(reg_range) = reg.next() {
                        current_virtio_reg = Some(reg_range);
                        // 检查是否同时有 reg 和 irq，如果有则创建设备
                        if let (Some(range), Some(irq)) = (current_virtio_reg.as_ref(), current_virtio_irq) {
                            if ans.virtio_count < 20 {
                                ans.virtio_devices[ans.virtio_count] = Some(VirtIODevice {
                                    base_addr: range.start,
                                    size: range.end - range.start,
                                    irq,
                                });
                                ans.virtio_count += 1;
                            }
                            current_virtio_reg = None;
                            current_virtio_irq = None;
                        }
                    }
                    WalkOperation::StepOver
                } else {
                    WalkOperation::StepOver
                }
            }
            DtbObj::Property(Property::General { name, value }) => {
                let node = ctx.name();
                if name == Str::from("timebase-frequency") {
                    ans.time_base_freq = bytes_to_usize(value) as u64;
                } else if name == Str::from("interrupts") && node.starts_with(VIRTIO) {
                    // VirtIO 设备的中断号
                    if let Some(first_4_bytes) = value.get(0..4) {
                        current_virtio_irq = Some(bytes_to_u32(first_4_bytes));
                        // 检查是否同时有 reg 和 irq，如果有则创建设备
                        if let (Some(range), Some(irq)) = (current_virtio_reg.as_ref(), current_virtio_irq) {
                            if ans.virtio_count < 20 {
                                ans.virtio_devices[ans.virtio_count] = Some(VirtIODevice {
                                    base_addr: range.start,
                                    size: range.end - range.start,
                                    irq,
                                });
                                ans.virtio_count += 1;
                            }
                            current_virtio_reg = None;
                            current_virtio_irq = None;
                        }
                    }
                }
                WalkOperation::StepOver
            }
            DtbObj::Property(_) => WalkOperation::StepOver,
        });
        ans
    }
}

fn bytes_to_usize(bytes: &[u8]) -> usize {
    let mut result = 0;
    for byte in bytes {
        result = (result << 8) | *byte as usize;
    }
    result
}

fn bytes_to_u32(bytes: &[u8]) -> u32 {
    let mut result = 0u32;
    for byte in bytes {
        result = (result << 8) | *byte as u32;
    }
    result
}
