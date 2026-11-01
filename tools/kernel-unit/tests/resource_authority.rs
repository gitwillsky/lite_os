//! Actual resource range authority; platform RAM/IRQ facts and object identity are host seams.
use native_abi::{ObjectType, resource, rights};
trait KernelObject {
    #[expect(
        dead_code,
        reason = "object identity dispatch is exercised by the kernel handle table, outside this range-authority fixture"
    )]
    fn object_type(&self) -> ObjectType;
    #[expect(
        dead_code,
        reason = "koid dispatch belongs to the kernel handle table rather than range delegation"
    )]
    fn koid(&self) -> u64;
    fn default_rights(&self) -> u32;
}
fn next_koid() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
mod platform {
    pub(crate) fn physical_memory_range() -> core::ops::Range<usize> {
        0x4000_0000..0x4800_0000
    }
    pub(crate) fn device_interrupt_range() -> core::ops::Range<u32> {
        32..256
    }
}
#[path = "../../../kernel/src/object/resource"]
mod authority {
    #[expect(
        dead_code,
        reason = "Resource koid is consumed by kernel object dispatch; this fixture exercises immutable kind/range authorization"
    )]
    mod authorization;
    pub(crate) use authorization::Resource;
}

#[test]
fn system_delegation_cannot_create_hardware_or_widen_itself() {
    let root = authority::Resource::root();
    let system = root
        .create_child(resource::SYSTEM, 0, resource::system::COUNT)
        .unwrap();
    assert!(!system.is_root());
    assert!(system.covers_system(resource::system::DEBUGLOG));
    assert!(system.covers_system(resource::system::SERIAL));
    assert!(system.covers_system(resource::system::UTC));
    assert!(system.covers_system(resource::system::POWER));
    assert!(!system.covers_mmio(0x0a00_0000, 4096));
    assert!(!system.covers_irq(48));
    assert_eq!(
        system.create_child(resource::MMIO, 0x0a00_0000, 4096).err(),
        Some(native_abi::ERR_ACCESS_DENIED)
    );
    assert_eq!(
        system.create_child(resource::IRQ, 48, 1).err(),
        Some(native_abi::ERR_ACCESS_DENIED)
    );
    assert_eq!(
        system.create_child(resource::ROOT, 0, 1).err(),
        Some(native_abi::ERR_INVALID_ARGS)
    );
    let log = system
        .create_child(resource::SYSTEM, resource::system::DEBUGLOG, 1)
        .unwrap();
    assert!(log.covers_system(resource::system::DEBUGLOG));
    assert!(!log.covers_system(resource::system::POWER));
    assert_eq!(
        log.create_child(resource::SYSTEM, 0, 2).err(),
        Some(native_abi::ERR_ACCESS_DENIED)
    );
    assert_eq!(
        root.create_child(resource::SYSTEM, 0, resource::system::COUNT + 1)
            .err(),
        Some(native_abi::ERR_INVALID_ARGS)
    );
    assert_ne!(
        system.default_rights() & rights::WRITE,
        0,
        "child delegation requires WRITE"
    );
}

#[test]
fn hardware_authority_stays_in_its_kind_and_cannot_map_ram() {
    let root = authority::Resource::root();
    assert_eq!(
        root.create_child(resource::MMIO, 0x4000_0000, 4096).err(),
        Some(native_abi::ERR_ACCESS_DENIED)
    );
    let mmio = root
        .create_child(resource::MMIO, 0x0a00_0000, 8192)
        .unwrap();
    assert!(mmio.covers_mmio(0x0a00_1000, 4096));
    assert!(!mmio.covers_mmio(0x0a00_2000, 4096));
    assert!(!mmio.covers_system(resource::system::POWER));
    assert_eq!(
        mmio.create_child(resource::SYSTEM, resource::system::POWER, 1)
            .err(),
        Some(native_abi::ERR_ACCESS_DENIED)
    );
    assert_eq!(
        root.create_child(resource::SYSTEM, u64::MAX, 1).err(),
        Some(native_abi::ERR_INVALID_ARGS)
    );
    assert_eq!(
        root.create_child(resource::SYSTEM, 0, 0).err(),
        Some(native_abi::ERR_INVALID_ARGS)
    );
}
