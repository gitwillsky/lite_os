//! AIA 的 immutable DTB 事实；CPU identity、interrupt-file 排列序号与 APLIC hart index 分别解码。
use alloc::vec::Vec;
use core::ops::Range;
use dtb_walker::{Context, DtbObj, Property, Str};

/// Supervisor APLIC 与 IMSIC 的完整装配输入；不包含可变路由状态。
pub(crate) struct AiaInfo {
    pub(crate) aplic: Range<usize>,
    pub(crate) sources: u32,
    pub(crate) identities: u32,
    pub(crate) windows: Vec<Range<usize>>,
    pub(crate) files: Vec<InterruptFile>,
}
/// 一个启用 CPU 的 supervisor interrupt file；不以 hardware CPU ID 推算地址。
pub(crate) struct InterruptFile {
    pub(crate) hardware_cpu: usize,
    pub(crate) address: usize,
    pub(crate) hart_index: u32,
}

#[derive(Clone, Copy)]
enum Kind {
    Cpu,
    CpuInterrupt,
    Aplic,
    Imsic,
}
#[derive(Default)]
struct Node {
    kind: Option<Kind>,
    depth: usize,
    registers: Vec<Range<usize>>,
    phandle: Option<u32>,
    parent: Option<u32>,
    interrupt_cells: Option<u32>,
    msi_cells: Option<u32>,
    controller: bool,
    msi: bool,
    sources: Option<u32>,
    identities: Option<u32>,
    extended: Vec<(u32, u32)>,
    guest_bits: u32,
    hart_bits: Option<u32>,
    group_bits: u32,
    group_shift: Option<u32>,
    disabled: bool,
    ssaia: bool,
}
/// 冷发现事务；节点属性完整提交后才解析跨 phandle 关系，不依赖属性或节点顺序。
#[derive(Default)]
pub(super) struct Discovery {
    node: Node,
    parent_cpu: Option<(usize, usize)>,
    cpus: Vec<(usize, bool, bool)>,
    cpu_interrupts: Vec<(u32, usize)>,
    aplics: Vec<Node>,
    imsics: Vec<Node>,
}
impl Discovery {
    pub(super) fn observe(&mut self, context: &Context<'_>, object: &DtbObj<'_>) {
        if matches!(object, DtbObj::SubNode { .. }) {
            self.finish_node();
            if self
                .parent_cpu
                .is_some_and(|(depth, _)| context.level() < depth)
            {
                self.parent_cpu = None;
            }
            return;
        }
        self.node.depth = context.level();
        if context.name().starts_with("cpu@") {
            self.node.kind = Some(Kind::Cpu);
        }
        match object {
            DtbObj::Property(Property::Compatible(values)) => {
                for value in values.clone() {
                    self.node.kind = self.node.kind.or_else(|| {
                        if value == Str::from("riscv,aplic") {
                            Some(Kind::Aplic)
                        } else if value == Str::from("riscv,imsics") {
                            Some(Kind::Imsic)
                        } else if value == Str::from("riscv,cpu-intc") {
                            Some(Kind::CpuInterrupt)
                        } else {
                            None
                        }
                    });
                }
            }
            DtbObj::Property(Property::Reg(registers)) => {
                for range in registers.clone() {
                    push(&mut self.node.registers, range);
                }
            }
            DtbObj::Property(Property::PHandle(value)) => self.node.phandle = Some(value.value()),
            DtbObj::Property(Property::Status(value)) => {
                self.node.disabled = *value != Str::from("okay") && *value != Str::from("ok");
            }
            DtbObj::Property(Property::General { name, value }) => {
                let scalar = || cell(value);
                if *name == Str::from("interrupt-controller") {
                    self.node.controller = true;
                } else if *name == Str::from("msi-controller") {
                    self.node.msi = true;
                } else if *name == Str::from("#interrupt-cells") {
                    self.node.interrupt_cells = Some(scalar());
                } else if *name == Str::from("#msi-cells") {
                    self.node.msi_cells = Some(scalar());
                } else if *name == Str::from("msi-parent") {
                    self.node.parent = Some(scalar());
                } else if *name == Str::from("riscv,num-sources") {
                    self.node.sources = Some(scalar());
                } else if *name == Str::from("riscv,num-ids") {
                    self.node.identities = Some(scalar());
                } else if *name == Str::from("riscv,guest-index-bits") {
                    self.node.guest_bits = scalar();
                } else if *name == Str::from("riscv,hart-index-bits") {
                    self.node.hart_bits = Some(scalar());
                } else if *name == Str::from("riscv,group-index-bits") {
                    self.node.group_bits = scalar();
                } else if *name == Str::from("riscv,group-index-shift") {
                    self.node.group_shift = Some(scalar());
                } else if *name == Str::from("interrupts-extended") {
                    assert!(
                        value.len().is_multiple_of(8),
                        "invalid CPU interrupt references"
                    );
                    for pair in value.as_chunks::<8>().0 {
                        push(
                            &mut self.node.extended,
                            (cell(&pair[..4]), cell(&pair[4..])),
                        );
                    }
                } else if *name == Str::from("status") {
                    self.node.disabled = *value != b"okay\0" && *value != b"ok\0";
                } else if *name == Str::from("riscv,isa-extensions") {
                    self.node.ssaia |= value.split(|byte| *byte == 0).any(|word| word == b"ssaia");
                } else if *name == Str::from("riscv,isa") {
                    self.node.ssaia |= value
                        .split(|byte| *byte == b'_' || *byte == 0)
                        .any(|word| word == b"ssaia");
                }
            }
            _ => {}
        }
    }
    pub(super) fn finish_node(&mut self) {
        let node = core::mem::take(&mut self.node);
        match node.kind {
            Some(Kind::Cpu) => {
                let id = node.registers.first().expect("CPU lacks reg").start;
                assert!(
                    !self.cpus.iter().any(|(old, _, _)| *old == id),
                    "duplicate CPU identity"
                );
                push(&mut self.cpus, (id, !node.disabled, node.ssaia));
                self.parent_cpu = Some((node.depth, id));
            }
            Some(Kind::CpuInterrupt) => {
                let (depth, id) = self
                    .parent_cpu
                    .expect("CPU interrupt controller lacks CPU parent");
                assert_eq!(
                    node.depth,
                    depth + 1,
                    "CPU interrupt controller has wrong parent"
                );
                let phandle = node
                    .phandle
                    .expect("CPU interrupt controller lacks phandle");
                assert!(
                    node.controller && node.interrupt_cells == Some(1),
                    "invalid CPU interrupt controller"
                );
                assert!(
                    !self.cpu_interrupts.iter().any(|(old, _)| *old == phandle),
                    "duplicate CPU interrupt phandle"
                );
                push(&mut self.cpu_interrupts, (phandle, id));
            }
            Some(Kind::Aplic) => push(&mut self.aplics, node),
            Some(Kind::Imsic) => push(&mut self.imsics, node),
            None => {}
        }
    }
    pub(super) fn cpu_enabled(&self, hardware: usize) -> bool {
        self.cpus
            .iter()
            .find(|(id, _, _)| *id == hardware)
            .expect("known CPU")
            .1
    }
    pub(super) fn finish(self, enabled: &[usize], uart_parent: Option<u32>) -> AiaInfo {
        assert!(
            enabled.iter().all(|hardware| self
                .cpus
                .iter()
                .any(|(id, active, ssaia)| id == hardware && *active && *ssaia)),
            "enabled CPU lacks Ssaia"
        );
        let mut supervisor = None;
        for node in self.imsics {
            if node.extended.iter().any(|(_, cause)| *cause == 9) {
                assert!(
                    node.extended
                        .iter()
                        .all(|(_, cause)| *cause == 9 || *cause == u32::MAX),
                    "mixed IMSIC privilege levels"
                );
                assert!(
                    supervisor.replace(node).is_none(),
                    "multiple supervisor IMSIC controllers"
                );
            }
        }
        let imsic = supervisor.expect("QEMU virt requires supervisor IMSIC");
        assert!(
            !imsic.disabled
                && imsic.controller
                && imsic.msi
                && imsic.interrupt_cells == Some(0)
                && imsic.msi_cells.is_none_or(|count| count == 0),
            "invalid IMSIC controller binding"
        );
        let identities = imsic.identities.expect("IMSIC lacks riscv,num-ids");
        assert!(
            (63..=2047).contains(&identities) && (identities + 1).is_multiple_of(64),
            "invalid IMSIC identities"
        );
        let phandle = imsic.phandle.expect("IMSIC lacks phandle");
        let hart_bits = imsic
            .hart_bits
            .unwrap_or_else(|| imsic.extended.len().next_power_of_two().trailing_zeros());
        let shift = imsic.group_shift.unwrap_or(24);
        assert!(
            imsic.guest_bits <= 7
                && hart_bits <= 15
                && imsic.group_bits <= 7
                && hart_bits + imsic.group_bits <= 14
                && (24..=55).contains(&shift)
                && (imsic.group_bits == 0 || 12 + imsic.guest_bits + hart_bits <= shift),
            "IMSIC address geometry cannot be encoded by APLIC"
        );
        assert!(
            !imsic.registers.is_empty() && imsic.registers.len() <= 1usize << imsic.group_bits,
            "invalid IMSIC groups"
        );
        let dynamic = mask(imsic.guest_bits + hart_bits, 12) | mask(imsic.group_bits, shift);
        let base = imsic.registers[0].start as u64 & !dynamic;
        for (index, range) in imsic.registers.iter().enumerate() {
            assert!(
                range.start != 0
                    && range.start < range.end
                    && range.start.is_multiple_of(4096)
                    && range.end.is_multiple_of(4096),
                "invalid IMSIC MMIO range"
            );
            assert!(
                imsic.registers[..index]
                    .iter()
                    .all(|old| old.end <= range.start || range.end <= old.start),
                "overlapping IMSIC groups"
            );
        }
        let mut files = Vec::new();
        for (index, (parent, cause)) in imsic.extended.iter().enumerate() {
            if *cause == u32::MAX {
                continue;
            }
            let hardware = self
                .cpu_interrupts
                .iter()
                .find(|(value, _)| value == parent)
                .expect("IMSIC references unknown CPU interrupt controller")
                .1;
            if !enabled.contains(&hardware) {
                continue;
            }
            assert!(
                !files
                    .iter()
                    .any(|file: &InterruptFile| file.hardware_cpu == hardware),
                "duplicate IMSIC CPU file"
            );
            let range = imsic
                .registers
                .get(index >> hart_bits)
                .expect("IMSIC group is missing");
            let local = index & ((1usize << hart_bits) - 1);
            let address = range
                .start
                .checked_add(local << (12 + imsic.guest_bits))
                .expect("IMSIC file overflow");
            assert!(
                address
                    .checked_add(4096)
                    .is_some_and(|end| end <= range.end)
                    && address as u64 & !dynamic == base,
                "IMSIC file exceeds declared geometry"
            );
            let hart_index = (((address as u64 >> shift) & mask(imsic.group_bits, 0)) << hart_bits)
                | ((address as u64 >> (12 + imsic.guest_bits)) & mask(hart_bits, 0));
            push(
                &mut files,
                InterruptFile {
                    hardware_cpu: hardware,
                    address,
                    hart_index: hart_index as u32,
                },
            );
        }
        assert_eq!(
            files.len(),
            enabled.len(),
            "enabled CPU lacks an IMSIC file"
        );
        let mut supervisor = None;
        for node in self.aplics {
            if node.parent == Some(phandle) {
                assert!(
                    supervisor.replace(node).is_none(),
                    "multiple supervisor APLIC domains"
                );
            }
        }
        let mut aplic = supervisor.expect("QEMU virt requires supervisor APLIC");
        let sources = aplic.sources.expect("APLIC lacks riscv,num-sources");
        assert!(
            !aplic.disabled
                && aplic.controller
                && aplic.interrupt_cells == Some(2)
                && (1..=1023).contains(&sources)
                && aplic.registers.len() == 1,
            "invalid APLIC domain"
        );
        assert!(
            aplic.phandle.is_some() && uart_parent == aplic.phandle,
            "UART must reference supervisor APLIC"
        );
        let aplic = aplic.registers.pop().expect("one APLIC range");
        assert!(
            aplic.start != 0
                && aplic.start.is_multiple_of(4096)
                && aplic
                    .end
                    .checked_sub(aplic.start)
                    .is_some_and(|size| size >= 0x4000),
            "invalid APLIC MMIO range"
        );
        AiaInfo {
            aplic,
            sources,
            identities,
            windows: imsic.registers,
            files,
        }
    }
}
fn mask(bits: u32, shift: u32) -> u64 {
    ((1u64 << bits) - 1) << shift
}
fn cell(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("DTB cell must be four bytes"))
}
fn push<T>(values: &mut Vec<T>, value: T) {
    values
        .try_reserve(1)
        .expect("AIA discovery allocation failed");
    values.push(value);
}
