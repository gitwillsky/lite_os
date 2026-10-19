//! RISC-V QEMU `virt` DTB handoff 与 immutable machine facts publication owner。

use dtb_walker::{Dtb, HeaderError};
use spin::Once;

use super::device_tree::PlatformInfo;
use crate::cpu::HardwareCpuId;

// OWNER: platform discovery publishes the immutable machine description for the kernel lifetime.
static PLATFORM_INFO: Once<PlatformInfo> = Once::new();

/// QEMU virt firmware 交付的 opaque device-tree handoff。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootInfo(usize);

impl BootInfo {
    pub(crate) fn from_firmware_opaque(value: usize) -> Self {
        Self(value)
    }

    pub(super) fn address(self) -> usize {
        self.0
    }
}

/// 解析 firmware 交付的 QEMU `virt` flattened device tree。
///
/// # Parameters
///
/// - `device_tree_address`: identity-mapped DTB physical address。
///
/// # Errors
///
/// DTB 无效或重复初始化时 fail-stop。
pub(crate) fn initialize(boot: BootInfo) {
    PLATFORM_INFO.call_once(|| {
        // SAFETY: firmware passes the physical DTB pointer unchanged in `a1`; early kernel
        // identity mapping covers it, and dtb-walker validates header and structure bounds.
        let dtb = unsafe {
            Dtb::from_raw_parts_filtered(boot.address() as *const u8, |error| {
                matches!(
                    error,
                    HeaderError::Misaligned(4) | HeaderError::LastCompVersion(_)
                )
            })
        }
        .expect("invalid RISC-V DTB");
        super::device_tree::parse(dtb, boot.address())
    });
}

pub(crate) fn validate_boot_info(boot: BootInfo) {
    assert_eq!(
        boot.address(),
        info().dtb.start,
        "secondary received a different platform handoff"
    );
}

/// 获取已发布的 immutable platform description。
///
/// # Returns
///
/// kernel lifetime 内唯一的 QEMU `virt` description。
///
/// # Errors
///
/// platform 尚未初始化时等待 publication。
pub(super) fn info() -> &'static PlatformInfo {
    PLATFORM_INFO.wait()
}

/// 迭代 platform 中所有 enabled hardware CPU identity。
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
