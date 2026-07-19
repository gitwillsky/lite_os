//! @description QEMU `virt` AArch64 DTB discovery 与 immutable machine facts owner。

use alloc::vec::Vec;
use core::{fmt, ops::Range};

use dtb_walker::{Dtb, DtbObj, HeaderError, Property, Str, WalkOperation};
use spin::Once;

use crate::cpu::HardwareCpuId;

const MAX_VIRTIO_DEVICES: usize = 32;

// OWNER: discovery publishes the only immutable AArch64 QEMU machine description.
static PLATFORM_INFO: Once<PlatformInfo> = Once::new();

/// @description AArch64 Linux boot protocol 在 `x0` 交付的 DTB physical address。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootInfo(usize);

impl BootInfo {
    /// @description 将 raw entry handoff 封装为 platform-owned token。
    pub(crate) fn from_firmware_opaque(value: usize) -> Self {
        Self(value)
    }

    pub(super) fn address(self) -> usize {
        self.0
    }
}

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
    hardware_cpu_ids: Vec<usize>,
    pub(crate) memory: Range<usize>,
    pub(crate) uart: MmioDevice,
    pub(crate) rtc: RangeValue,
    pub(crate) gic: GicV3Info,
    pub(crate) virtio_devices: [Option<MmioDevice>; MAX_VIRTIO_DEVICES],
    pub(crate) virtio_count: usize,
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
        writeln!(output, "VirtIO devices: {}", self.virtio_count)
    }
}

pub(crate) fn initialize(boot: BootInfo) {
    assert!(PLATFORM_INFO.get().is_none(), "platform initialized twice");
    PLATFORM_INFO.call_once(|| PlatformInfo::parse(boot.address()));
}

pub(crate) fn validate_boot_info(boot: BootInfo) {
    assert_eq!(
        boot.address(),
        info().dtb.start,
        "secondary received a different DTB handoff"
    );
}

pub(crate) fn info() -> &'static PlatformInfo {
    PLATFORM_INFO.wait()
}

pub(crate) fn info_if_initialized() -> Option<&'static PlatformInfo> {
    PLATFORM_INFO.get()
}

pub(crate) fn hardware_cpu_ids() -> impl ExactSizeIterator<Item = HardwareCpuId> {
    HardwareCpuIds(info().hardware_cpu_ids.iter())
}

struct HardwareCpuIds(core::slice::Iter<'static, usize>);

impl Iterator for HardwareCpuIds {
    type Item = HardwareCpuId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().copied().map(HardwareCpuId::from_raw)
    }
}

impl ExactSizeIterator for HardwareCpuIds {
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl PlatformInfo {
    fn parse(dtb_address: usize) -> Self {
        assert_ne!(dtb_address, 0, "AArch64 boot requires x0 DTB address");

        let mut hardware_cpu_ids = Vec::new();
        let mut memory = None;
        let mut uart_reg = None;
        let mut uart_irq = None;
        let mut rtc = None;
        let mut gic = None;
        let mut virtio_devices = [None; MAX_VIRTIO_DEVICES];
        let mut virtio_count = 0usize;
        let mut current_virtio_reg = None;
        let mut current_virtio_irq = None;
        let mut pl011_compatible = false;
        let mut pl031_compatible = false;
        let mut gicv3_compatible = false;
        let mut timer_compatible = false;
        let mut virtual_timer_ppi = false;
        let mut psci_compatible = false;
        let mut psci_hvc = false;
        let mut coherent_dma = false;

        let dtb_pointer = crate::arch::mmu::physical_to_virtual(dtb_address) as *const u8;
        // SAFETY: x0 follows the Linux arm64 boot ABI and the static TTBR1 direct map covers DTB;
        // dtb-walker validates header and structure bounds before exposing properties.
        let dtb = unsafe {
            Dtb::from_raw_parts_filtered(dtb_pointer, |error| {
                matches!(
                    error,
                    HeaderError::Misaligned(4) | HeaderError::LastCompVersion(_)
                )
            })
        }
        .expect("invalid AArch64 DTB");
        let dtb_range = dtb_address
            ..dtb_address
                .checked_add(dtb.total_size())
                .expect("DTB range overflow");

        dtb.walk(|context, object| match object {
            DtbObj::SubNode { name, .. } => {
                let parent = context.name();
                if context.is_root() {
                    if interesting_root_node(name) {
                        if name.starts_with("virtio_mmio") {
                            current_virtio_reg = None;
                            current_virtio_irq = None;
                        }
                        WalkOperation::StepInto
                    } else {
                        WalkOperation::StepOver
                    }
                } else if parent == Str::from("cpus") && name.starts_with("cpu@") {
                    WalkOperation::StepInto
                } else if parent == Str::from("soc") && interesting_device_node(name) {
                    if name.starts_with("virtio_mmio") {
                        current_virtio_reg = None;
                        current_virtio_irq = None;
                    }
                    WalkOperation::StepInto
                } else {
                    WalkOperation::StepOver
                }
            }
            DtbObj::Property(Property::Reg(mut registers)) => {
                let node = context.name();
                if node.starts_with("memory") {
                    memory = registers.next();
                } else if node.starts_with("cpu@") {
                    let hardware_id = registers.next().expect("CPU node lacks reg").start;
                    hardware_cpu_ids
                        .try_reserve(1)
                        .expect("CPU discovery allocation failed");
                    hardware_cpu_ids.push(hardware_id);
                } else if is_uart_node(node) {
                    uart_reg = registers.next();
                } else if is_rtc_node(node) {
                    rtc = registers.next().map(range_value);
                } else if is_gic_node(node) {
                    let distributor = registers.next().map(range_value);
                    let redistributor = registers.next().map(range_value);
                    if let (Some(distributor), Some(redistributor)) = (distributor, redistributor) {
                        gic = Some(GicV3Info {
                            distributor,
                            redistributor,
                        });
                    }
                } else if node.starts_with("virtio_mmio") {
                    current_virtio_reg = registers.next();
                    publish_virtio(
                        &mut virtio_devices,
                        &mut virtio_count,
                        &mut current_virtio_reg,
                        &mut current_virtio_irq,
                    );
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
                if name == Str::from("method") && is_psci_node(node) {
                    psci_hvc = contains_string(value, "hvc");
                } else if name == Str::from("interrupts") {
                    if is_uart_node(node) {
                        uart_irq = decode_first_gic_interrupt(value);
                    } else if node.starts_with("virtio_mmio") {
                        current_virtio_irq = decode_first_gic_interrupt(value);
                        publish_virtio(
                            &mut virtio_devices,
                            &mut virtio_count,
                            &mut current_virtio_reg,
                            &mut current_virtio_irq,
                        );
                    } else if is_timer_node(node) {
                        virtual_timer_ppi = contains_gic_interrupt(value, 1, 11);
                    }
                }
                WalkOperation::StepOver
            }
            DtbObj::Property(_) => WalkOperation::StepOver,
        });

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

        Self {
            dtb: dtb_range,
            hardware_cpu_ids,
            memory,
            uart,
            rtc,
            gic,
            virtio_devices,
            virtio_count,
        }
    }
}

fn interesting_root_node(name: Str<'_>) -> bool {
    name == Str::from("cpus")
        || name == Str::from("soc")
        || name.starts_with("memory")
        || interesting_device_node(name)
        || is_psci_node(name)
        || is_timer_node(name)
}

fn interesting_device_node(name: Str<'_>) -> bool {
    is_uart_node(name) || is_rtc_node(name) || is_gic_node(name) || name.starts_with("virtio_mmio")
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

fn publish_virtio(
    devices: &mut [Option<MmioDevice>; MAX_VIRTIO_DEVICES],
    count: &mut usize,
    register: &mut Option<Range<usize>>,
    irq: &mut Option<u32>,
) {
    let (Some(range), Some(vector)) = (register.as_ref(), *irq) else {
        return;
    };
    assert!(*count < MAX_VIRTIO_DEVICES, "too many VirtIO MMIO devices");
    assert!(valid_range(range), "invalid VirtIO MMIO range");
    devices[*count] = Some(MmioDevice {
        base_addr: range.start,
        size: range.end - range.start,
        irq: vector,
    });
    *count += 1;
    *register = None;
    *irq = None;
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
