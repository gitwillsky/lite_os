//! QEMU `virt` AArch64 DTB 到 immutable machine facts 的纯解码。
//!
//! 只解码内核自身需要的事实（内存、CPU、PL011、PL031、GICv3/ITS、timer、PSCI、initrd）；
//! 其余设备由用户态从同一 DTB 发现。ITS 只按 compatible 识别；PL011/PL031/GICv3 沿用名称候选与 compatible 断言。

use alloc::vec::Vec;
use core::{fmt, ops::Range};

use dtb_walker::{Dtb, DtbObj, Property, Str, WalkOperation};

#[derive(Debug, Clone, Copy)]
pub(crate) struct MmioDevice {
    pub(crate) base_addr: usize,
    pub(crate) size: usize,
    pub(crate) irq: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GicV3Info {
    pub(crate) distributor: RangeValue,
    pub(crate) redistributor: RangeValue,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RangeValue {
    pub(crate) start: usize,
    pub(crate) size: usize,
}

impl RangeValue {
    pub(crate) fn range(self) -> Range<usize> {
        self.start..self.start + self.size
    }
}

pub(crate) struct PlatformInfo {
    pub(crate) dtb: Range<usize>,
    pub(crate) hardware_cpu_ids: Vec<usize>,
    pub(crate) memory: Range<usize>,
    pub(crate) uart: MmioDevice,
    pub(crate) rtc: RangeValue,
    pub(crate) gic: GicV3Info,
    /// 可选的 GICv3 ITS；消息中断表在首次分配时才创建。
    pub(crate) its: Option<RangeValue>,
    /// `/chosen/bootargs`（kernel command line），不含结尾 NUL；没有该属性时为空。
    pub(crate) bootargs: Vec<u8>,
    /// `/chosen` 的 initrd 物理区间（DT chosen 绑定的 `linux,initrd-start/end`）；没有 initrd 时为 `None`。
    pub(crate) initrd: Option<Range<usize>>,
    /// `/chosen/rng-seed`：固件提供的启动熵；没有时为空。
    pub(crate) rng_seed: Vec<u8>,
}

impl fmt::Display for PlatformInfo {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(output, "DTB: {:#x?}", self.dtb)?;
        writeln!(output, "Hardware CPUs: {:?}", self.hardware_cpu_ids)?;
        writeln!(output, "Memory: {:#x?}", self.memory)?;
        writeln!(
            output,
            "PL011: {:#x}+{:#x}, IRQ {}",
            self.uart.base_addr, self.uart.size, self.uart.irq
        )?;
        writeln!(
            output,
            "GICv3: GICD={:#x}+{:#x}, GICR={:#x}+{:#x}",
            self.gic.distributor.start,
            self.gic.distributor.size,
            self.gic.redistributor.start,
            self.gic.redistributor.size
        )?;
        writeln!(output, "initrd: {:#x?}", self.initrd)
    }
}

/// 解码 AArch64 QEMU `virt` DTB。
///
/// # Parameters
///
/// - `dtb`: 已验证 header 与结构边界的 DTB。
/// - `physical`: DTB 的 physical start，用于发布 DTB 自身占用的区间。
///
/// # Returns
///
/// 完整的 immutable machine facts。
///
/// # Panics
///
/// 缺少 PL011、PL031、GICv3、virtual timer、PSCI HVC、coherent DMA 或 CPU 时 fail-stop。
/// 其余设备由用户态从同一 DTB 发现。
pub(crate) fn parse(dtb: Dtb<'_>, physical: usize) -> PlatformInfo {
    {
        let mut hardware_cpu_ids = Vec::new();
        let mut memory = None;
        let mut uart_reg = None;
        let mut uart_irq = None;
        let mut rtc = None;
        let mut gic = None;
        let mut its = None;
        let mut its_compatible = false;
        let mut its_reg = None;
        let mut initrd_start = None;
        let mut initrd_end = None;
        let mut rng_seed = Vec::new();
        let mut pl011_compatible = false;
        let mut pl031_compatible = false;
        let mut gicv3_compatible = false;
        let mut timer_compatible = false;
        let mut virtual_timer_ppi = false;
        let mut psci_compatible = false;
        let mut psci_hvc = false;
        let mut coherent_dma = false;
        let mut bootargs = Vec::new();

        let dtb_range = physical
            ..physical
                .checked_add(dtb.total_size())
                .expect("DTB range overflow");

        dtb.walk(|context, object| match object {
            DtbObj::SubNode { .. } => {
                commit_its(
                    &mut its,
                    its_reg.take(),
                    core::mem::take(&mut its_compatible),
                );
                WalkOperation::StepInto
            }
            DtbObj::Property(Property::Reg(mut registers)) => {
                let node = context.name();
                let first = registers.next();
                its_reg = first.clone().map(range_value);
                if node.starts_with("memory") {
                    memory = first;
                } else if node.starts_with("cpu@") {
                    let hardware_id = first.expect("CPU node lacks reg").start;
                    hardware_cpu_ids
                        .try_reserve(1)
                        .expect("CPU discovery allocation failed");
                    hardware_cpu_ids.push(hardware_id);
                } else if is_uart_node(node) {
                    uart_reg = first;
                } else if is_rtc_node(node) {
                    rtc = first.map(range_value);
                } else if is_gic_node(node) {
                    let distributor = first.map(range_value);
                    let redistributor = registers.next().map(range_value);
                    if let (Some(distributor), Some(redistributor)) = (distributor, redistributor) {
                        gic = Some(GicV3Info {
                            distributor,
                            redistributor,
                        });
                    }
                }
                WalkOperation::StepOver
            }
            DtbObj::Property(Property::Compatible(compatible)) => {
                let node = context.name();
                pl011_compatible |= is_uart_node(node)
                    && compatible
                        .clone()
                        .any(|value| value == Str::from("arm,pl011"));
                pl031_compatible |= is_rtc_node(node)
                    && compatible
                        .clone()
                        .any(|value| value == Str::from("arm,pl031"));
                its_compatible |= compatible
                    .clone()
                    .any(|value| value == Str::from("arm,gic-v3-its"));
                gicv3_compatible |= is_gic_node(node)
                    && compatible
                        .clone()
                        .any(|value| value == Str::from("arm,gic-v3"));
                timer_compatible |= is_timer_node(node)
                    && compatible
                        .clone()
                        .any(|value| value == Str::from("arm,armv8-timer"));
                psci_compatible |= is_psci_node(node)
                    && compatible.clone().any(|value| {
                        value == Str::from("arm,psci-1.0") || value == Str::from("arm,psci-0.2")
                    });
                WalkOperation::StepOver
            }
            DtbObj::Property(Property::DmaCoherent) => {
                coherent_dma = true;
                WalkOperation::StepOver
            }
            DtbObj::Property(Property::General { name, value }) => {
                let node = context.name();
                if name == Str::from("bootargs") && node == Str::from("chosen") {
                    let line = value.split(|byte| *byte == 0).next().unwrap_or(&[]);
                    bootargs
                        .try_reserve_exact(line.len())
                        .expect("bootargs allocation failed");
                    bootargs.extend_from_slice(line);
                } else if node == Str::from("chosen") && name == Str::from("rng-seed") {
                    rng_seed
                        .try_reserve_exact(value.len())
                        .expect("rng-seed allocation failed");
                    rng_seed.extend_from_slice(value);
                } else if node == Str::from("chosen") && name == Str::from("linux,initrd-start") {
                    initrd_start = decode_cell(value);
                } else if node == Str::from("chosen") && name == Str::from("linux,initrd-end") {
                    initrd_end = decode_cell(value);
                } else if name == Str::from("method") && is_psci_node(node) {
                    psci_hvc = contains_string(value, "hvc");
                } else if name == Str::from("interrupts") {
                    if is_uart_node(node) {
                        uart_irq = decode_first_gic_interrupt(value);
                    } else if is_timer_node(node) {
                        virtual_timer_ppi = contains_gic_interrupt(value, 1, 11);
                    }
                }
                WalkOperation::StepOver
            }
            DtbObj::Property(_) => WalkOperation::StepOver,
        });

        commit_its(&mut its, its_reg, its_compatible);

        assert!(pl011_compatible, "QEMU virt requires arm,pl011");
        assert!(pl031_compatible, "QEMU virt requires arm,pl031");
        assert!(gicv3_compatible, "QEMU virt requires arm,gic-v3");
        assert!(
            timer_compatible && virtual_timer_ppi,
            "virtual timer PPI 27 missing"
        );
        assert!(psci_compatible && psci_hvc, "PSCI HVC 0.2+ is required");
        assert!(coherent_dma, "AArch64 QEMU virt requires coherent DMA");
        assert!(!hardware_cpu_ids.is_empty(), "DTB contains no enabled CPUs");

        let memory = memory
            .filter(valid_range)
            .expect("DTB memory range missing");
        let uart_range = uart_reg.filter(valid_range).expect("PL011 reg missing");
        let uart = MmioDevice {
            base_addr: uart_range.start,
            size: uart_range.end - uart_range.start,
            irq: uart_irq.expect("PL011 interrupt missing"),
        };
        let rtc = rtc
            .filter(|range| valid_range_value(*range))
            .expect("PL031 reg missing");
        let gic = gic.expect("GICv3 distributor/redistributor ranges missing");
        assert!(
            valid_range_value(gic.distributor) && valid_range_value(gic.redistributor),
            "invalid GICv3 MMIO ranges"
        );
        let initrd = match (initrd_start, initrd_end) {
            (Some(start), Some(end)) if start < end => Some(start..end),
            _ => None,
        };

        PlatformInfo {
            dtb: dtb_range,
            hardware_cpu_ids,
            memory,
            uart,
            rtc,
            gic,
            its,
            bootargs,
            initrd,
            rng_seed,
        }
    }
}

/// 只提交同一节点的 compatible 与 reg；不以名称猜测身份，也不混用兄弟节点的属性。
fn commit_its(found: &mut Option<RangeValue>, reg: Option<RangeValue>, compatible: bool) {
    if compatible {
        let range = reg
            .filter(|range| valid_range_value(*range))
            .expect("ITS reg missing");
        assert!(
            found.replace(range).is_none(),
            "multiple ITS controllers are not supported"
        );
    }
}

fn is_uart_node(name: Str<'_>) -> bool {
    name.starts_with("pl011") || name.starts_with("uart")
}

fn is_rtc_node(name: Str<'_>) -> bool {
    name.starts_with("pl031") || name.starts_with("rtc")
}

fn is_gic_node(name: Str<'_>) -> bool {
    name.starts_with("intc") || name.starts_with("gic")
}

fn is_psci_node(name: Str<'_>) -> bool {
    name.starts_with("psci")
}

fn is_timer_node(name: Str<'_>) -> bool {
    name == Str::from("timer") || name.starts_with("timer@")
}

fn range_value(range: Range<usize>) -> RangeValue {
    RangeValue {
        start: range.start,
        size: range.end - range.start,
    }
}

fn valid_range(range: &Range<usize>) -> bool {
    range.start != 0 && range.end > range.start
}

fn valid_range_value(range: RangeValue) -> bool {
    range.start != 0 && range.size != 0 && range.start.checked_add(range.size).is_some()
}

fn contains_string(bytes: &[u8], expected: &str) -> bool {
    bytes
        .split(|byte| *byte == 0)
        .any(|value| value == expected.as_bytes())
}

fn decode_first_gic_interrupt(bytes: &[u8]) -> Option<u32> {
    let cells = bytes.get(..12)?;
    let interrupt_type = be_u32(&cells[..4]);
    let number = be_u32(&cells[4..8]);
    match interrupt_type {
        0 => number.checked_add(32),
        1 => number.checked_add(16),
        _ => None,
    }
}

fn contains_gic_interrupt(bytes: &[u8], expected_type: u32, expected_number: u32) -> bool {
    bytes.as_chunks::<12>().0.iter().any(|cells| {
        be_u32(&cells[..4]) == expected_type && be_u32(&cells[4..8]) == expected_number
    })
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("DTB cell must contain four bytes"))
}

/// 解码 `/chosen` 的 initrd 地址：DT 允许 32 位或 64 位单元。
fn decode_cell(bytes: &[u8]) -> Option<usize> {
    match bytes.len() {
        4 => Some(be_u32(bytes) as usize),
        8 => Some(be_u64(bytes) as usize),
        _ => None,
    }
}

fn be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(
        bytes
            .try_into()
            .expect("DTB value must contain eight bytes"),
    )
}
