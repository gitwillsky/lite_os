//! 以当前 host QEMU `dumpdtb` 输出为 fixture，验证生产 DTB 解码器。
//!
//! fixture 由 `verify_unit` 每次用与 runtime gate 相同的 machine 配置重新生成，因此 QEMU 升级
//! 改变节点命名或布局时，在 unit 阶段就会失败，而不是在 boot 时 panic。

use std::{env, fs, panic};

use dtb_walker::{Dtb, DtbObj, HeaderError, Property, WalkOperation};

use crate::qemu_virt::{aarch64, riscv64};

/// fixture 生成时使用的 `-smp`；验证 CPU 枚举不是只看第一个 hart。
const FIXTURE_CPUS: usize = 2;

fn fixture(variable: &str) -> Vec<u8> {
    let path = env::var_os(variable)
        .unwrap_or_else(|| panic!("{variable} is unset; run `make verify-unit`"));
    fs::read(&path).unwrap_or_else(|error| panic!("read {path:?}: {error}"))
}

fn dtb(bytes: &[u8]) -> Dtb<'_> {
    Dtb::from_slice_filtered(bytes, |error| {
        matches!(
            error,
            HeaderError::Misaligned(4) | HeaderError::LastCompVersion(_)
        )
    })
    .unwrap_or_else(|_| panic!("QEMU DTB fixture must be valid"))
}

/// 原位替换等长字节串；节点名与 compatible 都是 NUL 结尾字符串，等长替换不改变 DTB 结构。
fn replace_all(bytes: &mut [u8], needle: &[u8], replacement: &[u8]) {
    assert_eq!(needle.len(), replacement.len());
    let mut found = 0;
    let mut index = 0;
    while let Some(offset) = bytes[index..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let start = index + offset;
        bytes[start..start + needle.len()].copy_from_slice(replacement);
        index = start + needle.len();
        found += 1;
    }
    assert!(
        found > 0,
        "fixture lacks {:?}",
        String::from_utf8_lossy(needle)
    );
}

#[test]
fn riscv64_qemu_virt_dtb_decodes_every_boot_fact() {
    let bytes = fixture("LITEOS_RISCV64_DTB");
    let info = riscv64::device_tree::parse(dtb(&bytes), 0x8700_0000);
    assert_eq!(info.hardware_cpu_ids, [0, 1][..FIXTURE_CPUS]);
    assert_eq!(info.timebase_frequency, 10_000_000);
    assert_eq!(info.memory.start, 0x8000_0000);
    assert_eq!(info.uart, 0x1000_0000..0x1000_0100);
    assert_eq!(info.uart_irq, 10);
    assert_eq!(info.aia.aplic.start, 0x0d00_0000);
    assert_eq!(info.aia.identities, 255);
    assert_eq!(info.aia.files.len(), FIXTURE_CPUS);
    for hardware in 0..FIXTURE_CPUS {
        let file = info
            .aia
            .files
            .iter()
            .find(|file| file.hardware_cpu == hardware)
            .expect("each enabled CPU has a supervisor file");
        assert_eq!(file.address, 0x2800_0000 + hardware * 4096);
        assert_eq!(file.hart_index, hardware as u32);
    }
    assert_eq!(info.rtc, Some(0x0010_1000..0x0010_2000));
}

#[test]
fn riscv64_device_identity_does_not_depend_on_node_names() {
    let mut bytes = fixture("LITEOS_RISCV64_DTB");
    // 1. 先由生产 decoder 选择 supervisor 文件，再从原 DTB 定位它的节点。
    let selected = riscv64::device_tree::parse(dtb(&bytes), 0x8700_0000)
        .aia
        .windows[0]
        .start;
    let mut name = None;
    dtb(&bytes).walk(|context, object| {
        if let DtbObj::Property(Property::Reg(mut registers)) = object
            && registers
                .next()
                .is_some_and(|range| range.start == selected)
        {
            assert!(
                name.replace(context.name().as_bytes().to_vec()).is_none(),
                "one supervisor IMSIC node"
            );
        }
        WalkOperation::StepInto
    });
    let name = name.expect("fixture must contain supervisor IMSIC");
    // 2. 等长改名保持 DTB 结构与 compatible 不变；decoder 必须继续识别同一个设备。
    let replacement = vec![b'x'; name.len()];
    replace_all(&mut bytes, &name, &replacement);
    replace_all(&mut bytes, b"serial@10000000", b"tty000@10000000");
    replace_all(&mut bytes, b"rtc@101000", b"clk@101000");
    let info = riscv64::device_tree::parse(dtb(&bytes), 0x8700_0000);
    assert_eq!(info.aia.aplic.start, 0x0d00_0000);
    assert_eq!(info.uart_irq, 10);
    assert!(info.rtc.is_some());
}

#[test]
fn riscv64_missing_imsic_fails_with_a_reason() {
    let mut bytes = fixture("LITEOS_RISCV64_DTB");
    replace_all(&mut bytes, b"riscv,imsics", b"vendor,imsic");
    let panic = panic::catch_unwind(|| {
        riscv64::device_tree::parse(dtb(&bytes), 0x8700_0000);
    })
    .expect_err("a DTB without an IMSIC must fail-stop");
    let message = panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        message.contains("IMSIC"),
        "panic lacks the missing device: {message}"
    );
}

#[test]
fn aarch64_qemu_virt_dtb_decodes_every_boot_fact() {
    let bytes = fixture("LITEOS_AARCH64_DTB");
    let info = aarch64::device_tree::parse(dtb(&bytes), 0x4000_0000);
    assert_eq!(info.hardware_cpu_ids, [0, 1][..FIXTURE_CPUS]);
    assert_eq!(info.memory.start, 0x4000_0000);
    assert_eq!(info.uart.base_addr, 0x0900_0000);
    // PL011 是 SPI 1，GIC INTID = 32 + 1。
    assert_eq!(info.uart.irq, 33);
    assert_eq!(info.gic.distributor.start, 0x0800_0000);
    let its = info.its.expect("product machine enables ITS");
    assert_eq!(its.start, 0x0808_0000);
    assert_eq!(its.size, 0x20000);
}
