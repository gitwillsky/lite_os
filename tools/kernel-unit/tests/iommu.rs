//! Real IOMMU admission transactions; frame/page-table allocation is the host boundary, not hardware emulation.
#![feature(allocator_ext)]
extern crate alloc;
#[path = "../../../kernel/src/arch/dma.rs"]
#[expect(
    dead_code,
    reason = "hardware register backends are compiled but not executed in this host admission fixture"
)]
pub(crate) mod dma;
mod arch {
    pub(crate) use crate::dma;
    pub(crate) mod mmu {
        pub(crate) const USER_ADDRESS_END: usize = 1 << 38;
        pub(crate) const TRANSLATION_BITS: usize = 39;
        pub(crate) const MEMORY_ATTRIBUTES: u64 = 0;
        pub(crate) fn physical_to_virtual(address: usize) -> usize {
            address
        }
    }
}
#[expect(
    dead_code,
    reason = "DMA table allocation belongs to the guest hardware tests"
)]
mod memory {
    pub(crate) const PAGE_SIZE: usize = 4096;
    #[derive(Clone, Copy)]
    pub(crate) struct PhysicalPageNumber(usize);
    impl PhysicalPageNumber {
        pub(crate) fn as_usize(self) -> usize {
            self.0
        }
    }
    impl From<usize> for PhysicalPageNumber {
        fn from(value: usize) -> Self {
            Self(value)
        }
    }
    pub(crate) struct FrameTracker {
        pub(crate) ppn: PhysicalPageNumber,
    }
    pub(crate) enum FrameAllocationClass {
        Reclaimable,
    }
    pub(crate) fn alloc_contiguous(_: usize, _: FrameAllocationClass) -> Option<FrameTracker> {
        panic!("hardware factory is outside this admission fixture")
    }
}
mod execution {
    pub(crate) mod sync {
        pub(crate) struct TaskMutex<T>(spin::Mutex<T>);
        impl<T> TaskMutex<T> {
            pub(crate) fn new(value: T) -> Self {
                Self(spin::Mutex::new(value))
            }
            pub(crate) fn lock(&self) -> Result<spin::MutexGuard<'_, T>, ()> {
                Ok(self.0.lock())
            }
        }
    }
}
#[expect(
    dead_code,
    reason = "the host VM seam tests allocation admission, not device translation or MMIO mapping"
)]
mod vm {
    use crate::memory::PhysicalPageNumber;
    pub(crate) struct DeviceAddressSpace {
        root: alloc::boxed::Box<u8>,
        entries: alloc::vec::Vec<PhysicalPageNumber>,
    }
    pub(crate) struct Retired;
    impl Retired {
        pub(crate) fn release(self) {}
    }
    impl DeviceAddressSpace {
        pub(crate) fn create() -> Result<Self, ()> {
            Ok(Self {
                root: alloc::boxed::Box::try_new(0).map_err(|_| ())?,
                entries: alloc::vec::Vec::new(),
            })
        }
        pub(crate) fn root(&self) -> PhysicalPageNumber {
            PhysicalPageNumber::from(&*self.root as *const u8 as usize)
        }
        pub(crate) fn map(
            &mut self,
            _: usize,
            pages: &[PhysicalPageNumber],
            _: bool,
        ) -> Result<(), Retired> {
            self.entries.try_reserve(pages.len()).map_err(|_| Retired)?;
            self.entries.extend_from_slice(pages);
            Ok(())
        }
        pub(crate) fn unmap(&mut self, _: usize, _: usize) -> Retired {
            Retired
        }
    }
    pub(crate) struct KernelSpace;
    impl KernelSpace {
        pub(crate) fn map_device(&mut self, _: core::ops::Range<usize>) -> Result<usize, ()> {
            panic!("no guest MMIO in admission tests")
        }
    }
    pub(crate) static KERNEL_SPACE: spin::Once<spin::Mutex<KernelSpace>> = spin::Once::new();
}
#[path = "../../../kernel/src/iommu/mod.rs"]
#[expect(
    dead_code,
    reason = "this fixture executes generic map admission; identity and MMIO factories run in the guest"
)]
mod owner;
