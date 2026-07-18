//! @description Raw architecture callback codec and typed generic-kernel handoff。

use crate::{cpu::HardwareCpuId, platform::BootInfo};

/// @description 完整且已类型化的单 CPU firmware boot handoff。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootContext {
    hardware_cpu: HardwareCpuId,
    platform: BootInfo,
}

impl BootContext {
    /// @description 由 architecture entry 将 firmware register ABI 封装为 typed handoff。
    /// @param hardware_cpu platform 提供的 opaque hardware CPU identity。
    /// @param platform platform boot description 的 typed token。
    /// @return generic kernel 唯一消费的 boot context。
    pub(crate) fn new(hardware_cpu: HardwareCpuId, platform: BootInfo) -> Self {
        Self {
            hardware_cpu,
            platform,
        }
    }

    /// @description 投影本次 entry 的 hardware CPU identity。
    /// @return 不泄漏 raw integer 的 typed identity。
    pub(crate) fn hardware_cpu(self) -> HardwareCpuId {
        self.hardware_cpu
    }

    /// @description 投影本次 entry 的 platform boot token。
    /// @return 只由 platform façade 解释的 typed token。
    pub(crate) fn platform(self) -> BootInfo {
        self.platform
    }
}

/// Architecture primary callback；raw firmware registers 在此只转换一次。
#[unsafe(no_mangle)]
extern "C" fn __liteos_primary_entry(hardware_cpu: usize, platform_opaque: usize) -> ! {
    crate::kernel_main(BootContext::new(
        HardwareCpuId::from_raw(hardware_cpu),
        BootInfo::from_firmware_opaque(platform_opaque),
    ))
}

/// Architecture secondary callback；与 primary 复用同一 typed handoff。
#[unsafe(no_mangle)]
extern "C" fn __liteos_secondary_entry(hardware_cpu: usize, platform_opaque: usize) -> ! {
    crate::kernel_secondary_main(BootContext::new(
        HardwareCpuId::from_raw(hardware_cpu),
        BootInfo::from_firmware_opaque(platform_opaque),
    ))
}

/// Architecture user-trap callback；raw assembly ABI 不进入 generic trap domain。
#[unsafe(no_mangle)]
extern "C" fn __liteos_user_trap() -> ! {
    crate::trap::handle_user_trap()
}

/// Architecture kernel-trap callback；返回后由 backend assembly 恢复 interrupted context。
#[unsafe(no_mangle)]
extern "C" fn __liteos_kernel_trap() {
    crate::trap::handle_kernel_trap();
}
