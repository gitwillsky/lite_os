extern crate alloc;
// The real driver-service decoder must retain device facts after kernel DTB scope shrinks.
#[path = "../../../system/driver-manager/src/platform/device_tree.rs"]
mod device_tree;
#[path = "../../../system/driver-manager/src/platform/pci_host.rs"]
mod pci_host;
#[path = "../../../system/driver-manager/src/platform/virtio_mmio.rs"]
mod virtio_mmio;
use dtb_walker::{Dtb, HeaderError};

fn parse(bytes: &[u8]) -> device_tree::DeviceFacts {
    let dtb = Dtb::from_slice_filtered(bytes, |error| {
        matches!(
            error,
            HeaderError::Misaligned(4) | HeaderError::LastCompVersion(_)
        )
    })
    .unwrap_or_else(|_| panic!("QEMU DTB fixture must be valid"));
    device_tree::parse(dtb)
}

#[cfg(target_arch = "aarch64")]
#[test]
fn actual_qemu_device_windows_and_routes_belong_to_driver_host() {
    let bytes =
        std::fs::read(std::env::var_os("LITEOS_AARCH64_DTB").expect("run make verify-unit"))
            .unwrap();
    let facts = parse(&bytes);
    assert_eq!(facts.virtio.iter().count(), 32);
    assert!(
        facts
            .virtio
            .iter()
            .all(|transport| transport.size == 0x200 && transport.irq >= 32)
    );
    let pci = facts.pci.unwrap();
    assert!(!pci.ecam.is_empty() && !pci.mmio32.is_empty());
    assert!(pci.interrupt(0, 1).is_some());
    assert_eq!(pci.interrupt(0, 0), None);
    assert_eq!(pci.stream(0), None);
    assert!(facts.iommu.is_none());
}

#[cfg(target_arch = "aarch64")]
#[test]
fn compatible_identity_survives_qemu_device_node_renaming() {
    let mut bytes =
        std::fs::read(std::env::var_os("LITEOS_AARCH64_DTB").expect("run make verify-unit"))
            .unwrap();
    let original = parse(&bytes);
    let needle = b"virtio_mmio@";
    let mut renamed = 0;
    for offset in 0..bytes.len() - needle.len() {
        if &bytes[offset..offset + needle.len()] == needle {
            bytes[offset..offset + needle.len()].fill(b'x');
            renamed += 1;
        }
    }
    assert_eq!(renamed, original.virtio.iter().count());
    let renamed = parse(&bytes);
    assert_eq!(
        original.virtio.iter().collect::<Vec<_>>(),
        renamed.virtio.iter().collect::<Vec<_>>()
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn actual_smmu_phandle_routes_pci_and_retains_mmio_transports() {
    let bytes =
        std::fs::read(std::env::var_os("LITEOS_AARCH64_IOMMU_DTB").expect("run make verify-unit"))
            .unwrap();
    let facts = parse(&bytes);
    assert_eq!(facts.iommu.unwrap().kind, native_abi::iommu::ARM_SMMU_V3);
    assert_eq!(facts.virtio.iter().count(), 32);
    let pci = facts.pci.unwrap();
    assert_eq!(pci.stream(0), Some(0));
    assert_eq!(pci.stream(255), Some(255));
    assert_eq!(pci.stream(65535), Some(65535));
    assert!(std::panic::catch_unwind(|| pci.stream(65536)).is_err());
}

#[test]
fn pci_stream_mask_and_disjoint_range_boundaries_do_not_bypass_translation() {
    let pci = pci_host::PciHost::new(
        0..4096,
        0..4096,
        [[0; 4]; 4],
        vec![(8, 100, 8), (32, 200, 2)],
        0xff,
    );
    assert_eq!(pci.stream(0x108), Some(100));
    assert_eq!(pci.stream(15), Some(107));
    assert_eq!(pci.stream(32), Some(200));
    assert_eq!(pci.stream(33), Some(201));
    for requester in [7, 16, 31, 34] {
        assert!(std::panic::catch_unwind(|| pci.stream(requester)).is_err());
    }
}
