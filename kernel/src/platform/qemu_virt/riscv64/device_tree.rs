//! QEMU `virt` RISC-V DTB 到 immutable machine facts 的纯解码。
//!
//! 设备身份只由 `compatible` 裁决；`memory`、`cpu@` 与 `/cpus` 是 DTB 规范强制的节点名，
//! 仍按名称识别。节点属性在下一个 `SubNode` 事件或遍历结束时提交，与属性顺序无关。

use alloc::vec::Vec;
use core::{fmt, ops::Range};

use dtb_walker::{Dtb, DtbObj, Property, Str, StrList, WalkOperation};

use super::super::virtio_mmio::{VirtioMmioScan, VirtioMmioTransports};

/// Linux `ns16550a` earlycon/serial 驱动匹配的 QEMU UART。
const UART_COMPATIBLES: &[&str] = &["ns16550a"];
/// Linux `irq-sifive-plic` 匹配的 PLIC；QEMU 11 之前节点名为 `plic@…`，之后为
/// `interrupt-controller@…`，hart-local `riscv,cpu-intc` 也使用后者，因此只能按 compatible 区分。
const PLIC_COMPATIBLES: &[&str] = &["sifive,plic-1.0.0", "riscv,plic0"];
/// Linux `rtc-goldfish` 匹配的 QEMU RTC。
const RTC_COMPATIBLES: &[&str] = &["google,goldfish-rtc"];

/// 由 DTB 一次性解码的 RISC-V QEMU `virt` machine facts。
pub(crate) struct PlatformInfo {
    pub(crate) dtb: Range<usize>,
    pub(crate) hardware_cpu_ids: Vec<usize>,
    pub(crate) timebase_frequency: u64,
    pub(crate) memory: Range<usize>,
    pub(crate) uart: Range<usize>,
    pub(crate) uart_irq: u32,
    pub(crate) plic: Range<usize>,
    pub(crate) rtc: Option<Range<usize>>,
    pub(crate) virtio: VirtioMmioTransports,
}

impl fmt::Display for PlatformInfo {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(output, "DTB: {:#x?}", self.dtb)?;
        writeln!(output, "Hardware CPUs: {:?}", self.hardware_cpu_ids)?;
        writeln!(output, "Timebase: {} Hz", self.timebase_frequency)?;
        writeln!(output, "Memory: {:#x?}", self.memory)?;
        writeln!(output, "UART: {:#x?}, IRQ {}", self.uart, self.uart_irq)?;
        writeln!(output, "PLIC: {:#x?}", self.plic)?;
        writeln!(output, "RTC: {:#x?}", self.rtc)?;
        writeln!(output, "VirtIO-MMIO transports: {}", self.virtio.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    Uart,
    Plic,
    Rtc,
}

#[derive(Default)]
struct PendingDevice {
    kind: Option<DeviceKind>,
    reg: Option<Range<usize>>,
    irq: Option<u32>,
}

#[derive(Default)]
struct Devices {
    uart: Option<(Range<usize>, Option<u32>)>,
    plic: Option<Range<usize>>,
    rtc: Option<Range<usize>>,
}

impl Devices {
    /// 提交上一个节点；只有 compatible 已识别的节点才进入 machine facts。
    ///
    /// # Panics
    ///
    /// 已识别设备缺少合法 `reg` 时 fail-stop，避免把 broken DTB 降级为“设备不存在”。
    fn commit(&mut self, pending: PendingDevice) {
        let Some(kind) = pending.kind else {
            return;
        };
        let reg = pending
            .reg
            .filter(valid_range)
            .unwrap_or_else(|| panic!("{kind:?} DTB node lacks a valid reg window"));
        match kind {
            // QEMU 只公开一个 16550；若出现多个，与 Linux stdout-path 缺省一致取首个。
            DeviceKind::Uart => {
                self.uart.get_or_insert((reg, pending.irq));
            }
            DeviceKind::Plic => {
                assert!(self.plic.is_none(), "DTB declares more than one PLIC");
                self.plic = Some(reg);
            }
            DeviceKind::Rtc => {
                self.rtc.get_or_insert(reg);
            }
        }
    }
}

/// 解码 RISC-V QEMU `virt` DTB。
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
/// 缺少 memory、CPU、timebase、UART/UART interrupt 或 PLIC，或已识别设备的 `reg`
/// 非法时 fail-stop；这些都是 boot 必需事实，降级运行只会在后续以无上下文的方式失败。
pub(crate) fn parse(dtb: Dtb<'_>, physical: usize) -> PlatformInfo {
    let dtb_range = physical
        ..physical
            .checked_add(dtb.total_size())
            .expect("DTB range overflow");
    let mut hardware_cpu_ids = Vec::new();
    let mut timebase_frequency = 0;
    let mut memory = None;
    let mut devices = Devices::default();
    let mut pending = PendingDevice::default();
    let mut virtio = VirtioMmioScan::new();

    dtb.walk(|context, object| match object {
        DtbObj::SubNode { .. } => {
            devices.commit(core::mem::take(&mut pending));
            virtio.begin_node();
            WalkOperation::StepInto
        }
        DtbObj::Property(Property::Compatible(values)) => {
            pending.kind = classify(values.clone());
            virtio.compatible(values);
            WalkOperation::StepOver
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
            } else {
                let first = registers.next();
                pending.reg = first.clone();
                virtio.reg(first);
            }
            WalkOperation::StepOver
        }
        DtbObj::Property(Property::General { name, value }) => {
            if name == Str::from("timebase-frequency") && context.name() == Str::from("cpus") {
                timebase_frequency = be_uint(value);
            } else if name == Str::from("interrupts") {
                // PLIC `#interrupt-cells = <1>`：首个 cell 即 source id。
                let irq = value.get(..4).map(|cell| be_uint(cell) as u32);
                pending.irq = pending.irq.or(irq);
                virtio.interrupt(irq);
            }
            WalkOperation::StepOver
        }
        DtbObj::Property(_) => WalkOperation::StepOver,
    });
    devices.commit(pending);

    assert!(!hardware_cpu_ids.is_empty(), "DTB contains no CPUs");
    assert_ne!(timebase_frequency, 0, "DTB /cpus lacks timebase-frequency");
    let memory = memory
        .filter(valid_range)
        .expect("DTB memory range missing");
    let (uart, uart_irq) = devices.uart.expect("QEMU virt requires an ns16550a UART");
    PlatformInfo {
        dtb: dtb_range,
        hardware_cpu_ids,
        timebase_frequency,
        memory,
        uart,
        uart_irq: uart_irq
            .filter(|irq| *irq != 0)
            .expect("ns16550a UART lacks a PLIC interrupt"),
        plic: devices
            .plic
            .expect("QEMU virt requires a sifive,plic-1.0.0/riscv,plic0 interrupt controller"),
        rtc: devices.rtc,
        virtio: virtio.finish(),
    }
}

fn classify(values: StrList<'_>) -> Option<DeviceKind> {
    for value in values {
        for (kind, compatibles) in [
            (DeviceKind::Uart, UART_COMPATIBLES),
            (DeviceKind::Plic, PLIC_COMPATIBLES),
            (DeviceKind::Rtc, RTC_COMPATIBLES),
        ] {
            if compatibles
                .iter()
                .any(|expected| value == Str::from(*expected))
            {
                return Some(kind);
            }
        }
    }
    None
}

fn valid_range(range: &Range<usize>) -> bool {
    range.start != 0 && range.end > range.start
}

/// 解码 big-endian 的 1 或 2 cell 无符号整数。
fn be_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
}
