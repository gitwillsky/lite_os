//! QEMU `virt` RISC-V DTB 到 immutable machine facts 的纯解码。
//!
//! 设备身份只由 `compatible` 裁决；`memory`、`cpu@` 与 `/cpus` 是 DTB 规范强制的节点名，
//! 仍按名称识别。节点属性在下一个 `SubNode` 事件或遍历结束时提交，与属性顺序无关。

mod aia;
pub(crate) use aia::AiaInfo;

use alloc::vec::Vec;
use core::{fmt, ops::Range};

use dtb_walker::{Dtb, DtbObj, Property, Str, StrList, WalkOperation};

/// compatible 为 `ns16550a` 的 QEMU UART。
const UART_COMPATIBLES: &[&str] = &["ns16550a"];
/// compatible 为 `google,goldfish-rtc` 的 QEMU RTC。
const RTC_COMPATIBLES: &[&str] = &["google,goldfish-rtc"];

/// 由 DTB 一次性解码的 RISC-V QEMU `virt` machine facts。
pub(crate) struct PlatformInfo {
    pub(crate) dtb: Range<usize>,
    pub(crate) hardware_cpu_ids: Vec<usize>,
    pub(crate) timebase_frequency: u64,
    pub(crate) memory: Range<usize>,
    pub(crate) uart: Range<usize>,
    pub(crate) uart_irq: u32,
    pub(crate) aia: AiaInfo,
    pub(crate) rtc: Option<Range<usize>>,
    /// `/chosen` 的 initrd 物理区间（DT chosen 绑定的 `linux,initrd-start/end`）；没有 initrd 时为 `None`。
    pub(crate) initrd: Option<Range<usize>>,
    /// `/chosen/rng-seed`：固件提供的启动熵；没有时为空。
    pub(crate) rng_seed: Vec<u8>,
    /// `/chosen/bootargs`（kernel command line），不含结尾 NUL；没有该属性时为空。
    pub(crate) bootargs: Vec<u8>,
}

impl fmt::Display for PlatformInfo {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(output, "DTB: {:#x?}", self.dtb)?;
        writeln!(output, "Hardware CPUs: {:?}", self.hardware_cpu_ids)?;
        writeln!(output, "Timebase: {} Hz", self.timebase_frequency)?;
        writeln!(output, "Memory: {:#x?}", self.memory)?;
        writeln!(output, "UART: {:#x?}, IRQ {}", self.uart, self.uart_irq)?;
        writeln!(
            output,
            "APLIC: {:#x?}, IMSIC IDs: {}",
            self.aia.aplic, self.aia.identities
        )?;
        writeln!(output, "RTC: {:#x?}", self.rtc)?;
        writeln!(output, "initrd: {:#x?}", self.initrd)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    Uart,
    Rtc,
}

#[derive(Default)]
struct PendingDevice {
    kind: Option<DeviceKind>,
    reg: Option<Range<usize>>,
    irq: Option<u32>,
    irq_flags: Option<u32>,
    irq_parent: Option<u32>,
}

#[derive(Default)]
struct Devices {
    uart: Option<(Range<usize>, Option<u32>, Option<u32>)>,
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
            // QEMU 只公开一个 16550；若出现多个，取首个。
            DeviceKind::Uart => {
                assert_eq!(
                    pending.irq_flags,
                    Some(4),
                    "UART requires level-high APLIC interrupt"
                );
                self.uart
                    .get_or_insert((reg, pending.irq, pending.irq_parent));
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
/// 缺少 memory、CPU、timebase、UART/UART interrupt 或 APLIC/IMSIC，或已识别设备的 `reg`
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
    let mut aia = aia::Discovery::default();
    let mut initrd_start = None;
    let mut initrd_end = None;
    let mut rng_seed = Vec::new();
    let mut bootargs = Vec::new();

    dtb.walk(|context, object| {
        aia.observe(context, &object);
        match object {
            DtbObj::SubNode { .. } => {
                devices.commit(core::mem::take(&mut pending));
                WalkOperation::StepInto
            }
            DtbObj::Property(Property::Compatible(values)) => {
                pending.kind = classify(values.clone());
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
                    pending.reg = registers.next();
                }
                WalkOperation::StepOver
            }
            DtbObj::Property(Property::General { name, value }) => {
                if name == Str::from("bootargs") && context.name() == Str::from("chosen") {
                    let line = value.split(|byte| *byte == 0).next().unwrap_or(&[]);
                    bootargs
                        .try_reserve_exact(line.len())
                        .expect("bootargs allocation failed");
                    bootargs.extend_from_slice(line);
                } else if context.name() == Str::from("chosen") && name == Str::from("rng-seed") {
                    rng_seed
                        .try_reserve_exact(value.len())
                        .expect("rng-seed allocation failed");
                    rng_seed.extend_from_slice(value);
                } else if context.name() == Str::from("chosen")
                    && name == Str::from("linux,initrd-start")
                {
                    initrd_start = Some(be_uint(value) as usize);
                } else if context.name() == Str::from("chosen")
                    && name == Str::from("linux,initrd-end")
                {
                    initrd_end = Some(be_uint(value) as usize);
                } else if name == Str::from("timebase-frequency")
                    && context.name() == Str::from("cpus")
                {
                    timebase_frequency = be_uint(value);
                } else if name == Str::from("interrupt-parent") {
                    pending.irq_parent = Some(be_uint(value) as u32);
                } else if name == Str::from("interrupts") {
                    // APLIC 两 cell；到 compatible 提交时验证 UART，不依赖节点名或属性顺序。
                    if value.len() == 8 {
                        pending.irq = Some(be_uint(&value[..4]) as u32);
                        pending.irq_flags = Some(be_uint(&value[4..]) as u32);
                    }
                }
                WalkOperation::StepOver
            }
            DtbObj::Property(_) => WalkOperation::StepOver,
        }
    });
    devices.commit(pending);
    aia.finish_node();
    hardware_cpu_ids.retain(|id| aia.cpu_enabled(*id));
    let (uart, uart_irq, uart_parent) = devices.uart.expect("QEMU virt requires an ns16550a UART");
    let aia = aia.finish(&hardware_cpu_ids, uart_parent);

    assert!(!hardware_cpu_ids.is_empty(), "DTB contains no CPUs");
    assert_ne!(timebase_frequency, 0, "DTB /cpus lacks timebase-frequency");
    let memory = memory
        .filter(valid_range)
        .expect("DTB memory range missing");
    PlatformInfo {
        dtb: dtb_range,
        hardware_cpu_ids,
        timebase_frequency,
        memory,
        uart,
        uart_irq: uart_irq
            .filter(|irq| *irq != 0 && *irq <= aia.sources)
            .expect("ns16550a UART lacks an APLIC interrupt"),
        aia,
        rtc: devices.rtc,
        bootargs,
        rng_seed,
        initrd: match (initrd_start, initrd_end) {
            (Some(start), Some(end)) if start < end => Some(start..end),
            _ => None,
        },
    }
}

fn classify(values: StrList<'_>) -> Option<DeviceKind> {
    for value in values {
        for (kind, compatibles) in [
            (DeviceKind::Uart, UART_COMPATIBLES),
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
