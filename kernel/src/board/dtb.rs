use core::{
    fmt::{self, Display},
    ops::Range,
};

use dtb_walker::{Dtb, DtbObj, HeaderError, Property, Str, WalkOperation};

/// 在栈上存储有限长度字符串
pub struct StringInLine<const N: usize>(usize, [u8; N]);

pub struct BoardInfo {
    pub dtb: Range<usize>,
    pub model: StringInLine<128>,
    pub smp: usize,
    pub time_base_freq: u64,
    pub mem: Range<usize>,
    pub uart: Range<usize>,
    pub test: Range<usize>,
    pub clint: Range<usize>,
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

        let mut ans = BoardInfo {
            dtb: dtb_addr..dtb_addr,
            model: StringInLine(0, [0; 128]),
            smp: 0,
            mem: 0..0,
            uart: 0..0,
            test: 0..0,
            clint: 0..0,
            time_base_freq: 0,
        };

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
                    } else {
                        WalkOperation::StepOver
                    }
                } else if current == Str::from(SOC) {
                    if name.starts_with(UART)
                        || name.starts_with(TEST)
                        || name.starts_with(CLINT)
                        || name.starts_with(SERIAL)
                    {
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
                } else {
                    WalkOperation::StepOver
                }
            }
            DtbObj::Property(Property::General { name, value }) => {
                if name == Str::from("timebase-frequency") {
                    ans.time_base_freq = bytes_to_usize(value) as u64;
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
