//! QEMU virt 的 M-mode AIA 装配；S-mode 只接管已委托的 APLIC domain。
use core::{arch::asm, ops::Range};
use dtb_walker::{Dtb, DtbObj, Property, Str, WalkOperation};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Cpu,
    Imsic,
    Aplic,
}

#[derive(Default)]
struct Node {
    kind: Option<Kind>,
    registers: Option<Range<usize>>,
    phandle: Option<u32>,
    parent: Option<u32>,
    sources: u32,
    causes: u32,
    files: usize,
    guest: u32,
    hart: Option<u32>,
    group: u32,
    shift: Option<u32>,
    child: Option<u32>,
    delegated: [u32; 32],
    foreign_delegation: bool,
    disabled: bool,
    smaia: bool,
    ssaia: bool,
    stateen: bool,
}

/// 在 cold-boot hart 发布 SBI 服务前初始化根 APLIC；不分配堆内存。
///
/// # Parameters
/// `dtb` 是启动 ABI 交付且已验证的机器描述。
/// # Panics
/// 控制器关系、地址几何、委托范围或寄存器读回不符合本产品的 AIA 装配时 fail-stop。
pub(crate) fn initialize(dtb: &Dtb<'_>) {
    // 1. 按标准 cause 区分 M/S IMSIC，再按 msi-parent 解析 APLIC；不依赖地址或节点名。
    let mut machine = None;
    let mut supervisor = None;
    scan(dtb, None, |node| {
        if node.kind == Some(Kind::Imsic) {
            let destination = match node.causes {
                value if value == 1 << 11 => &mut machine,
                value if value == 1 << 9 => &mut supervisor,
                _ => panic!("IMSIC has invalid privilege targets"),
            };
            assert!(!node.disabled, "IMSIC is disabled");
            assert!(destination.replace(node).is_none(), "duplicate IMSIC");
        }
    });
    let machine = machine.expect("firmware requires machine IMSIC");
    let supervisor = supervisor.expect("firmware requires supervisor IMSIC");
    assert!(
        machine.phandle.is_some()
            && supervisor.phandle.is_some()
            && machine.phandle != supervisor.phandle,
        "invalid IMSIC phandles"
    );
    let mut root = None;
    let mut child = None;
    scan(dtb, None, |node| {
        if node.kind == Some(Kind::Aplic) {
            assert!(!node.disabled, "APLIC is disabled");
            let destination = if node.parent == machine.phandle {
                &mut root
            } else if node.parent == supervisor.phandle {
                &mut child
            } else {
                panic!("APLIC references unknown IMSIC");
            };
            assert!(
                destination.replace(node).is_none(),
                "duplicate APLIC domain"
            );
        }
    });
    let root = root.expect("firmware requires root APLIC");
    assert!(root.phandle.is_some(), "root APLIC lacks phandle");
    let child = child.expect("firmware requires supervisor APLIC");
    let child_phandle = child.phandle.expect("supervisor APLIC lacks phandle");
    let mut delegation = None;
    scan(dtb, Some(child_phandle), |node| {
        if node.kind == Some(Kind::Aplic) && node.phandle == root.phandle {
            assert!(delegation.replace(node).is_none(), "duplicate root APLIC");
        }
    });
    let delegation = delegation.expect("root APLIC lacks phandle");
    let child_index = delegation.child.expect("supervisor is not an APLIC child");
    assert!(child_index < 1024 && !delegation.foreign_delegation);
    assert!((1..=1023).contains(&root.sources) && child.sources == root.sources);
    for source in 1..=root.sources {
        assert_ne!(
            delegation.delegated[source as usize / 32] & (1 << (source % 32)),
            0,
            "QEMU firmware requires all external sources delegated to supervisor"
        );
    }
    // 2. MSI 高位几何共用根 mmsiaddrcfgh，M/S 的 guest stride 可不同。
    let (machine_base, machine_high, hart, group, shift) = geometry(&machine);
    let (supervisor_base, supervisor_high, s_hart, s_group, s_shift) = geometry(&supervisor);
    assert_eq!(
        (hart, group, shift),
        (s_hart, s_group, s_shift),
        "M/S IMSIC group geometry differs"
    );
    let window = root.registers.expect("root APLIC lacks reg");
    assert!(
        window.start != 0
            && window.start.is_multiple_of(4096)
            && window
                .end
                .checked_sub(window.start)
                .is_some_and(|size| size >= 0x4000)
    );
    let base = window.start;
    write(base, 0, 4);
    for word in 0..=root.sources / 32 {
        write(base, 0x1f00 + word as usize * 4, u32::MAX);
        write(base, 0x1d00 + word as usize * 4, u32::MAX);
    }
    assert_eq!(
        read(base, 0x1bc4) & (1 << 31),
        0,
        "MSI geometry locked before firmware setup"
    );
    write(base, 0x1bc0, machine_base);
    write(base, 0x1bc4, machine_high);
    write(base, 0x1bc8, supervisor_base);
    write(base, 0x1bcc, supervisor_high & 0x0070_0fff);
    for (offset, value) in [
        (0x1bc0, machine_base),
        (0x1bc4, machine_high),
        (0x1bc8, supervisor_base),
        (0x1bcc, supervisor_high & 0x0070_0fff),
    ] {
        assert_eq!(read(base, offset), value, "APLIC MSI geometry rejected");
    }
    // 3. 完成可读配置校验后锁定目标地址，再逐源委托；S-mode 稍后独占配置其 domain。
    write(base, 0x1bc4, machine_high | (1 << 31));
    assert_ne!(
        read(base, 0x1bc4) & (1 << 31),
        0,
        "APLIC MSI geometry did not lock"
    );
    for source in 1..=root.sources {
        let offset = source as usize * 4;
        write(base, offset, 1 << 10 | child_index);
        assert_eq!(
            read(base, offset),
            1 << 10 | child_index,
            "APLIC delegation rejected"
        );
    }
}

/// 为当前 hart 开放 Ssaia CSR；只在 DTB 声明 Smstateen 时访问 mstateen0。
/// # Parameters
/// `dtb` 是已验证机器描述；`hart` 是当前硬件 hart ID。
/// # Panics
/// 当前 CPU 未提供 Smaia/Ssaia 或拒绝开放 CSR 时 fail-stop。
pub(crate) fn initialize_local(dtb: &Dtb<'_>, hart: usize) {
    let mut found = false;
    scan(dtb, None, |node| {
        if node.kind != Some(Kind::Cpu)
            || node.registers.as_ref().is_none_or(|reg| reg.start != hart)
        {
            return;
        }
        assert!(
            !found && !node.disabled && node.smaia && node.ssaia,
            "hart lacks AIA"
        );
        found = true;
        if node.stateen {
            // SAFETY: 每 hart 的 M-mode 初始化独占此 CSR；DTB 保证 Smstateen 存在。
            // AIA/IMSIC/CSRIND 缺一会使 S-mode 的 siselect/sireg/stopei 访问陷入固件。
            let enabled: usize;
            let required = (1usize << 58) | (1 << 59) | (1 << 60);
            unsafe {
                asm!("csrs 0x30c, {required}", "csrr {enabled}, 0x30c",
                required = in(reg) required, enabled = out(reg) enabled, options(nomem));
            }
            assert_eq!(enabled & required, required, "Ssaia CSR access rejected");
        }
    });
    assert!(found, "current hart absent from DTB");
}

fn geometry(node: &Node) -> (u32, u32, u32, u32, u32) {
    let address = node.registers.as_ref().expect("IMSIC lacks reg").start as u64;
    assert_ne!(node.files, 0);
    let hart = node
        .hart
        .unwrap_or_else(|| node.files.next_power_of_two().trailing_zeros());
    let shift = node.shift.unwrap_or(24);
    assert!(
        node.guest <= 7
            && hart <= 15
            && node.group <= 7
            && hart + node.group <= 14
            && (24..=55).contains(&shift)
            && (node.group == 0 || 12 + node.guest + hart <= shift)
    );
    let dynamic = ((1u64 << (node.guest + hart)) - 1) << 12 | ((1u64 << node.group) - 1) << shift;
    let ppn = (address & !dynamic) >> 12;
    assert!(address != 0 && address.is_multiple_of(4096) && address >> 56 == 0);
    let high =
        (ppn >> 32) as u32 | node.guest << 20 | hart << 12 | node.group << 16 | (shift - 24) << 24;
    (ppn as u32, high, hart, node.group, shift)
}

fn scan(dtb: &Dtb<'_>, child: Option<u32>, mut consume: impl FnMut(Node)) {
    let mut node = Node::default();
    dtb.walk(|context, object| {
        if matches!(object, DtbObj::SubNode { .. }) {
            consume(core::mem::take(&mut node));
        } else {
            if context.name().starts_with("cpu@") {
                node.kind = Some(Kind::Cpu);
            }
            match object {
                DtbObj::Property(Property::Compatible(values)) => {
                    for value in values {
                        if value == Str::from("riscv,imsics") {
                            node.kind = Some(Kind::Imsic);
                        } else if value == Str::from("riscv,aplic") {
                            node.kind = Some(Kind::Aplic);
                        }
                    }
                }
                DtbObj::Property(Property::Reg(mut values)) => node.registers = values.next(),
                DtbObj::Property(Property::PHandle(value)) => node.phandle = Some(value.value()),
                DtbObj::Property(Property::Status(value)) => {
                    node.disabled = value != Str::from("okay") && value != Str::from("ok");
                }
                DtbObj::Property(Property::General { name, value }) => {
                    if name == Str::from("msi-parent") {
                        node.parent = Some(cell(value));
                    } else if name == Str::from("riscv,num-sources") {
                        node.sources = cell(value);
                    } else if name == Str::from("riscv,hart-index-bits") {
                        node.hart = Some(cell(value));
                    } else if name == Str::from("riscv,guest-index-bits") {
                        node.guest = cell(value);
                    } else if name == Str::from("riscv,group-index-bits") {
                        node.group = cell(value);
                    } else if name == Str::from("riscv,group-index-shift") {
                        node.shift = Some(cell(value));
                    } else if name == Str::from("status") {
                        node.disabled = value != b"okay\0" && value != b"ok\0";
                    } else if name == Str::from("interrupts-extended") {
                        assert!(value.len().is_multiple_of(8));
                        node.files = value.len() / 8;
                        for pair in value.as_chunks::<8>().0 {
                            let cause = cell(&pair[4..]);
                            if cause != u32::MAX {
                                node.causes |= if cause < 32 { 1 << cause } else { u32::MAX };
                            }
                        }
                    } else if name == Str::from("riscv,children") && child.is_some() {
                        assert!(value.len().is_multiple_of(4));
                        for (index, bytes) in value.as_chunks::<4>().0.iter().enumerate() {
                            if Some(cell(bytes)) == child {
                                assert!(
                                    node.child.replace(index as u32).is_none(),
                                    "duplicate APLIC child"
                                );
                            }
                        }
                    } else if name == Str::from("riscv,delegation") && child.is_some() {
                        assert!(value.len().is_multiple_of(12));
                        for triple in value.as_chunks::<12>().0 {
                            let (target, first, last) =
                                (cell(&triple[..4]), cell(&triple[4..8]), cell(&triple[8..]));
                            assert!(first != 0 && first <= last && last <= 1023);
                            node.foreign_delegation |= Some(target) != child;
                            for source in first..=last {
                                let word = &mut node.delegated[source as usize / 32];
                                let bit = 1 << (source % 32);
                                assert_eq!(*word & bit, 0, "overlapping APLIC delegation");
                                *word |= bit;
                            }
                        }
                    } else if name == Str::from("riscv,isa-extensions")
                        || name == Str::from("riscv,isa")
                    {
                        for extension in value.split(|byte| *byte == 0 || *byte == b'_') {
                            node.smaia |= extension == b"smaia";
                            node.ssaia |= extension == b"ssaia";
                            node.stateen |= extension == b"smstateen";
                        }
                    }
                }
                _ => {}
            }
        }
        WalkOperation::StepInto
    });
    consume(node);
}
fn cell(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("DTB cell must be four bytes"))
}
fn read(base: usize, offset: usize) -> u32 {
    // SAFETY: 唯一 cold-boot owner 校验了根 APLIC 的物理窗口，offset 均为对齐的标准寄存器。
    unsafe { ((base + offset) as *const u32).read_volatile() }
}
fn write(base: usize, offset: usize, value: u32) {
    // SAFETY: 唯一 cold-boot owner 独占根 APLIC 初始化；窗口及标准寄存器偏移已验证。
    unsafe {
        ((base + offset) as *mut u32).write_volatile(value);
        asm!("fence iorw, iorw");
    }
}
