//! QEMU `virt` AArch64 DTB handoff 与 immutable machine facts publication owner。

use dtb_walker::{Dtb, HeaderError};
use spin::Once;

use super::device_tree::PlatformInfo;
use crate::cpu::HardwareCpuId;

// OWNER: discovery publishes the only immutable AArch64 QEMU machine description.
static PLATFORM_INFO: Once<PlatformInfo> = Once::new();

/// AArch64 Linux boot protocol 在 `x0` 交付的 DTB physical address。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootInfo(usize);

impl BootInfo {
    /// 将 raw entry handoff 封装为 platform-owned token。
    pub(crate) fn from_firmware_opaque(value: usize) -> Self {
        Self(value)
    }

    pub(super) fn address(self) -> usize {
        self.0
    }
}

pub(crate) fn initialize(boot: BootInfo) {
    assert!(PLATFORM_INFO.get().is_none(), "platform initialized twice");
    PLATFORM_INFO.call_once(|| {
        assert_ne!(boot.address(), 0, "AArch64 boot requires x0 DTB address");
        let dtb_pointer = crate::arch::mmu::physical_to_virtual(boot.address()) as *const u8;
        // SAFETY: x0 follows the Linux arm64 boot ABI and the static TTBR1 direct map covers DTB;
        // dtb-walker validates header and structure bounds before exposing properties.
        let dtb = unsafe {
            Dtb::from_raw_parts_filtered(dtb_pointer, |error| {
                matches!(
                    error,
                    HeaderError::Misaligned(4) | HeaderError::LastCompVersion(_)
                )
            })
        }
        .expect("invalid AArch64 DTB");
        super::device_tree::parse(dtb, boot.address())
    });
}

pub(crate) fn validate_boot_info(boot: BootInfo) {
    assert_eq!(
        boot.address(),
        info().dtb.start,
        "secondary received a different DTB handoff"
    );
}

pub(super) fn info() -> &'static PlatformInfo {
    PLATFORM_INFO.wait()
}

pub(super) fn info_if_initialized() -> Option<&'static PlatformInfo> {
    PLATFORM_INFO.get()
}

pub(crate) fn hardware_cpu_ids() -> impl ExactSizeIterator<Item = HardwareCpuId> {
    HardwareCpuIds(info().hardware_cpu_ids.iter())
}

struct HardwareCpuIds(core::slice::Iter<'static, usize>);

impl Iterator for HardwareCpuIds {
    type Item = HardwareCpuId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().copied().map(HardwareCpuId::from_raw)
    }
}

impl ExactSizeIterator for HardwareCpuIds {
    fn len(&self) -> usize {
        self.0.len()
    }
}
