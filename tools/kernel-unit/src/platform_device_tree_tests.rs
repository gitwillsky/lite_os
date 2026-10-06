//! 以当前 host QEMU `dumpdtb` 输出为 fixture，验证生产 DTB 解码器。
//!
//! fixture 由 `verify_unit` 每次用与 runtime gate 相同的 machine 配置重新生成，因此 QEMU 升级
//! 改变节点命名或布局时，在 unit 阶段就会失败，而不是在 boot 时 panic。

use std::{env, fs, panic};

use dtb_walker::{Dtb, HeaderError};

use crate::qemu_virt::{aarch64, riscv64, virtio_mmio::VirtioMmioTransport};

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

fn riscv_transport_irqs(transports: impl Iterator<Item = VirtioMmioTransport>) -> Vec<u32> {
    let mut irqs: Vec<u32> = transports.map(|transport| transport.irq).collect();
    irqs.sort_unstable();
    irqs
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
    assert_eq!(info.plic.start, 0x0c00_0000);
    assert_eq!(info.rtc, Some(0x0010_1000..0x0010_2000));
    // QEMU virt 固定 8 个 VirtIO-MMIO slot，PLIC source 1..=8。
    assert_eq!(
        riscv_transport_irqs(info.virtio.iter().copied()),
        (1..=8).collect::<Vec<_>>()
    );
    assert_eq!(info.virtio.span(), Some(0x1000_1000..0x1000_9000));
}

#[test]
fn riscv64_device_identity_does_not_depend_on_node_names() {
    let mut bytes = fixture("LITEOS_RISCV64_DTB");
    // QEMU 11 把 PLIC 改名为 interrupt-controller@；任何节点再次改名都不能让设备消失。
    replace_all(
        &mut bytes,
        b"interrupt-controller@c000000",
        b"renamed-controller-x@c000000",
    );
    replace_all(&mut bytes, b"serial@10000000", b"tty000@10000000");
    replace_all(&mut bytes, b"virtio_mmio@", b"transport-x@");
    replace_all(&mut bytes, b"rtc@101000", b"clk@101000");
    let info = riscv64::device_tree::parse(dtb(&bytes), 0x8700_0000);
    assert_eq!(info.plic.start, 0x0c00_0000);
    assert_eq!(info.uart_irq, 10);
    assert_eq!(info.virtio.len(), 8);
    assert!(info.rtc.is_some());
}

#[test]
fn riscv64_missing_plic_fails_with_a_reason() {
    let mut bytes = fixture("LITEOS_RISCV64_DTB");
    replace_all(&mut bytes, b"sifive,plic-1.0.0", b"vendor,intc-1.0.0");
    replace_all(&mut bytes, b"riscv,plic0", b"vendor,pic0");
    let panic = panic::catch_unwind(|| {
        riscv64::device_tree::parse(dtb(&bytes), 0x8700_0000);
    })
    .expect_err("a DTB without a PLIC must fail-stop");
    let message = panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        message.contains("plic"),
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
    assert!(info.pci.interrupt(1, 1).is_some());
    // QEMU virt 固定 32 个 VirtIO-MMIO slot，SPI 16..=47（INTID 48..=79）。
    let mut irqs: Vec<u32> = info.virtio.iter().map(|transport| transport.irq).collect();
    irqs.sort_unstable();
    assert_eq!(irqs, (48..80).collect::<Vec<_>>());
    assert_eq!(info.virtio.span(), Some(0x0a00_0000..0x0a00_4000));
}

#[test]
fn aarch64_virtio_identity_does_not_depend_on_node_names() {
    let mut bytes = fixture("LITEOS_AARCH64_DTB");
    replace_all(&mut bytes, b"virtio_mmio@", b"transport-x@");
    let info = aarch64::device_tree::parse(dtb(&bytes), 0x4000_0000);
    assert_eq!(info.virtio.len(), 32);
}
