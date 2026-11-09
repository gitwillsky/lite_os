//! Admission tests execute the generic owner; host VM/frame/IRQ boundaries are supplied by kernel-unit.
use super::*;
use core::sync::atomic::AtomicUsize;

#[path = "../../../tools/kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;

struct MockHardware {
    attached: AtomicUsize,
}
impl Hardware for MockHardware {
    fn attach(&self, _: u32, _: u16, _: usize) -> Result<(), Status> {
        self.attached.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn invalidate(&self, _: u16, _: usize, _: usize) {}
    fn drain_faults(&self, _: &mut dyn FnMut(u32, u64)) -> bool {
        false
    }
}
fn owner(last_domain: u16) -> Iommu<MockHardware> {
    Iommu {
        hardware: MockHardware {
            attached: AtomicUsize::new(0),
        },
        domains: TaskMutex::new(FallibleMap::new()),
        last_domain: AtomicU16::new(last_domain),
        unattributed: AtomicU64::new(0),
    }
}

#[test]
fn allocation_failure_cannot_publish_an_unowned_hardware_root() {
    let mut successful_allocations = 0;
    loop {
        let owner = owner(0);
        let mut output = Vec::new();
        allocation_failure::fail_after(successful_allocations);
        let result = owner.map(
            7,
            &[PhysicalPageNumber::from(1)],
            true,
            CachePolicy::Cached,
            &mut output,
        );
        let consumed = allocation_failure::was_consumed();
        if owner.hardware.attached.load(Ordering::Relaxed) != 0 {
            assert!(
                owner.domains.lock().unwrap().contains_key(&7),
                "hardware obtained a root without its domain owner"
            );
        }
        if result.is_err() {
            assert!(output.is_empty());
        }
        if !consumed {
            assert!(result.is_ok(), "fault-free map must reach publication");
            assert_eq!(owner.hardware.attached.load(Ordering::Relaxed), 1);
            break;
        }
        successful_allocations += 1;
    }
}

#[test]
fn domain_identity_exhaustion_cannot_wrap_and_reuse_a_live_tag() {
    let owner = owner(u16::MAX);
    let mut output = Vec::new();
    for stream in [1, 2] {
        assert_eq!(
            owner.map(
                stream,
                &[PhysicalPageNumber::from(1)],
                true,
                CachePolicy::Cached,
                &mut output
            ),
            Err(native_abi::ERR_NO_RESOURCES)
        );
    }
    assert_eq!(owner.hardware.attached.load(Ordering::Relaxed), 0);
    assert_eq!(owner.last_domain.load(Ordering::Relaxed), u16::MAX);
}
