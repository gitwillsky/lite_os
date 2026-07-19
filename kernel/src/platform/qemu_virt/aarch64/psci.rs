//! @description PSCI 0.2+ HVC conduit owner for QEMU `virt`。

use core::{arch::asm, fmt};

const PSCI_VERSION: u32 = 0x8400_0000;
const PSCI_CPU_ON_64: u32 = 0xc400_0003;
const PSCI_SYSTEM_OFF: u32 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u32 = 0x8400_0009;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FirmwareError(i32);

impl fmt::Display for FirmwareError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "PSCI error {}", self.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CpuStartError(FirmwareError);

impl fmt::Display for CpuStartError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResetError(FirmwareError);

impl fmt::Display for ResetError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}", self.0)
    }
}

pub(crate) fn verify() {
    let version = call(PSCI_VERSION, [0, 0, 0]);
    assert!(version >= 0, "PSCI_VERSION failed: {version}");
    let version = version as u32;
    let major = version >> 16;
    let minor = version & 0xffff;
    assert!(major > 0 || minor >= 2, "PSCI 0.2 or newer required");
}

pub(crate) fn start_cpu(
    hardware_cpu_id: crate::cpu::HardwareCpuId,
    entry_address: usize,
    boot: super::BootInfo,
) -> Result<(), CpuStartError> {
    result(call(
        PSCI_CPU_ON_64,
        [hardware_cpu_id.raw(), entry_address, boot.address()],
    ))
    .map_err(CpuStartError)
}

pub(crate) fn reset_system(reset_type: usize, _reset_reason: usize) -> Result<(), ResetError> {
    let function = match reset_type {
        0 => PSCI_SYSTEM_OFF,
        1 => PSCI_SYSTEM_RESET,
        _ => return Err(ResetError(FirmwareError(-2))),
    };
    result(call(function, [0, 0, 0])).map_err(ResetError)
}

fn result(value: i32) -> Result<(), FirmwareError> {
    if value == 0 {
        Ok(())
    } else {
        Err(FirmwareError(value))
    }
}

#[inline]
fn call(function: u32, arguments: [usize; 3]) -> i32 {
    let mut result = function as usize;
    // SAFETY: DTB 已验证 PSCI `method = "hvc"`；PSCI 使用 x0..x3 传参且允许覆盖
    // AAPCS64 caller-saved x0..x17。调用不暴露指针，firmware 返回值只从 x0 解码。
    unsafe {
        asm!(
            "hvc #0",
            inout("x0") result,
            in("x1") arguments[0],
            in("x2") arguments[1],
            in("x3") arguments[2],
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x8") _,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x13") _,
            lateout("x14") _,
            lateout("x15") _,
            lateout("x16") _,
            lateout("x17") _,
            options(nostack)
        );
    }
    result as i32
}
