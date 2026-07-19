const EID_TIME: usize = 0x5449_4d45;
const EID_IPI: usize = 0x0073_5049;
const EID_RFENCE: usize = 0x5246_4e43;
const EID_SYSTEM_RESET: usize = 0x5352_5354;
const EID_DEBUG_CONSOLE: usize = 0x4442_434e;
const EID_HSM: usize = 0x0048_534d;
const EID_BASE: usize = 0x10;

const FID_SET_TIMER: usize = 0;
const FID_SEND_IPI: usize = 0;
const FID_REMOTE_FENCE_I: usize = 0;
const FID_REMOTE_SFENCE_VMA: usize = 1;
const FID_SYSTEM_RESET: usize = 0;
const FID_CONSOLE_WRITE: usize = 0;
const FID_CONSOLE_WRITE_BYTE: usize = 2;
const FID_HART_START: usize = 0;
const FID_PROBE_EXTENSION: usize = 3;

/// @description SBI operation failure retained only inside the platform implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FirmwareError {
    code: isize,
}

impl core::fmt::Display for FirmwareError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "firmware operation failed with SBI status {}",
            self.code
        )
    }
}

/// @description Secondary CPU start failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CpuStartError(FirmwareError);

impl core::fmt::Display for CpuStartError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "CPU start failed: {}", self.0)
    }
}

/// @description Local timer programming failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimerArmError(FirmwareError);

impl core::fmt::Display for TimerArmError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "timer arm failed: {}", self.0)
    }
}

/// @description Synchronous remote TLB invalidation failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TlbShootdownError(FirmwareError);

impl core::fmt::Display for TlbShootdownError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "TLB shootdown failed: {}", self.0)
    }
}

/// @description 同步远端 instruction fetch 失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InstructionFenceError(FirmwareError);

impl core::fmt::Display for InstructionFenceError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "remote instruction fence failed: {}", self.0)
    }
}

/// @description Whole-system reset request failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResetError(FirmwareError);

impl core::fmt::Display for ResetError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "system reset failed: {}", self.0)
    }
}

/// @description 执行 SBI v0.2+ EID/FID 调用。
///
/// @param eid SBI extension ID，写入 `a7`。
/// @param fid SBI function ID，写入 `a6`。
/// @param args 六个 XLEN 参数，依次写入 `a0..a5`。
/// @return `(error, value)`，分别来自 `a0` 和 `a1`。
#[inline(always)]
fn sbi_call(eid: usize, fid: usize, args: [usize; 6]) -> (isize, usize) {
    let error: isize;
    let value: usize;
    // SAFETY: registers follow the SBI calling convention, `ecall` transfers to trusted
    // M-mode firmware, and the assembly neither dereferences memory nor touches the stack.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("x17") eid,
            in("x16") fid,
            inlateout("x10") args[0] => error,
            inlateout("x11") args[1] => value,
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
            options(nostack),
        );
    }
    (error, value)
}

#[inline(always)]
fn value_or_error(error: isize, value: usize) -> Result<usize, FirmwareError> {
    if error == 0 {
        Ok(value)
    } else {
        Err(FirmwareError { code: error })
    }
}

fn probe_extension(eid: usize) -> Result<bool, FirmwareError> {
    let (error, value) = sbi_call(EID_BASE, FID_PROBE_EXTENSION, [eid, 0, 0, 0, 0, 0]);
    value_or_error(error, value).map(|value| value != 0)
}

/// @description 验证 kernel 启动与 fail-stop 路径依赖的 SBI extension。
///
/// @return 全部 extension 可用时返回；缺失或 probe 失败触发 kernel panic。
pub(crate) fn verify_firmware() {
    for (eid, name) in [
        (EID_TIME, "TIME"),
        (EID_IPI, "IPI"),
        (EID_RFENCE, "RFENCE"),
        (EID_SYSTEM_RESET, "SRST"),
        (EID_DEBUG_CONSOLE, "DBCN"),
        (EID_HSM, "HSM"),
    ] {
        assert!(
            probe_extension(eid).unwrap_or(false),
            "required SBI extension {name} ({eid:#x}) is unavailable"
        );
    }
}

/// @description 通过 SBI HSM 启动一个 DTB secondary hart。
///
/// @param hardware_cpu_id 目标 RISC-V hart identity。
/// @param start_address 目标 hart 的 S-mode 入口物理地址。
/// @param opaque 原样传给目标 hart `a1` 的 DTB 地址。
/// @return firmware 接受启动请求时返回 `Ok(())`，否则返回 SBI error。
pub(crate) fn start_cpu(
    hardware_cpu_id: crate::cpu::HardwareCpuId,
    start_address: usize,
    boot: super::BootInfo,
) -> Result<(), CpuStartError> {
    let (error, value) = sbi_call(
        EID_HSM,
        FID_HART_START,
        [
            hardware_cpu_id.raw(),
            start_address,
            boot.address(),
            0,
            0,
            0,
        ],
    );
    value_or_error(error, value)
        .map(|_| ())
        .map_err(CpuStartError)
}

/// @description 通过 SBI DBCN 写出单字节，不使用 legacy console extension。
///
/// @param byte 待写出的字节。
/// @return 成功返回 `Ok(())`；firmware 拒绝或不支持时返回 SBI error。
pub(crate) fn debug_console_write(byte: u8) -> Result<(), FirmwareError> {
    let (error, value) = sbi_call(
        EID_DEBUG_CONSOLE,
        FID_CONSOLE_WRITE_BYTE,
        [byte as usize, 0, 0, 0, 0, 0],
    );
    value_or_error(error, value).map(|_| ())
}

/// @description 通过 SBI DBCN bulk write 同步写出 identity-mapped kernel bytes。
/// @param bytes 位于 platform DRAM identity mapping 内的非空/空连续字节。
/// @return firmware 完整消费全部字节时成功；SBI error、零进度或越界进度时失败。
pub(crate) fn debug_console_write_bytes(bytes: &[u8]) -> Result<(), FirmwareError> {
    let mut written = 0usize;
    while written < bytes.len() {
        let address = bytes.as_ptr() as usize + written;
        let remaining = bytes.len() - written;
        let (error, value) = sbi_call(
            EID_DEBUG_CONSOLE,
            FID_CONSOLE_WRITE,
            [remaining, address, 0, 0, 0, 0],
        );
        let count = value_or_error(error, value)?;
        if count == 0 || count > remaining {
            // Firmware violated the DBCN progress contract. Reusing a standard SBI error code
            // keeps this failure inside the platform adapter; normal logging is best-effort.
            return Err(FirmwareError { code: -1 });
        }
        written += count;
    }
    Ok(())
}

/// @description 通过 SBI TIME 设置当前 hart 的绝对 timer deadline。
///
/// @param timer_value `time` CSR 同一计数域中的绝对值。
/// @return 成功返回 `Ok(())`，失败返回 SBI error。
pub(crate) fn arm_timer(timer_value: u64) -> Result<(), TimerArmError> {
    let (error, value) = sbi_call(
        EID_TIME,
        FID_SET_TIMER,
        [timer_value as usize, 0, 0, 0, 0, 0],
    );
    value_or_error(error, value)
        .map(|_| ())
        .map_err(TimerArmError)
}

/// @description 通过 SBI IPI 向 hart mask 发送 supervisor software interrupt。
///
/// @param hart_mask 从 `hart_mask_base` 开始的 hart 位图。
/// @param hart_mask_base 位图 bit 0 对应的 hart ID。
/// @return 成功返回 `Ok(())`，失败返回 SBI error。
pub(crate) fn send_ipi(cpus: crate::cpu::CpuSet) -> Result<(), FirmwareError> {
    for_each_hardware_mask(cpus, |mask, base| {
        let (error, value) = sbi_call(EID_IPI, FID_SEND_IPI, [mask, base, 0, 0, 0, 0]);
        value_or_error(error, value).map(|_| ())
    })
}

/// @description 请求目标 hart 同步完成 `SFENCE.VMA`。
///
/// @param hart_mask 从 `hart_mask_base` 开始的 hart 位图。
/// @param hart_mask_base 位图 bit 0 对应的 hart ID。
/// @param start_address 刷新区间起始虚拟地址；与 `size` 同为零表示全局刷新。
/// @param size 刷新区间字节数；与 `start_address` 同为零表示全局刷新。
/// @return SBI 仅在所有目标 hart 完成 fence 后返回 `Ok(())`；失败返回 SBI error。
pub(crate) fn synchronize_tlb(
    cpus: crate::cpu::CpuSet,
    start_address: usize,
    size: usize,
) -> Result<(), TlbShootdownError> {
    for_each_hardware_mask(cpus, |mask, base| {
        let (error, value) = sbi_call(
            EID_RFENCE,
            FID_REMOTE_SFENCE_VMA,
            [mask, base, start_address, size, 0, 0],
        );
        value_or_error(error, value)
            .map(|_| ())
            .map_err(TlbShootdownError)
    })
}

/// @description 请求目标 hart 同步完成 `FENCE.I`。
/// @param cpus 需要观察 instruction publication 的 logical CPU 集合。
/// @return SBI 在全部目标完成后返回成功；失败返回 firmware error。
pub(crate) fn synchronize_instruction_cache(
    cpus: crate::cpu::CpuSet,
) -> Result<(), InstructionFenceError> {
    for_each_hardware_mask(cpus, |mask, base| {
        let (error, value) = sbi_call(EID_RFENCE, FID_REMOTE_FENCE_I, [mask, base, 0, 0, 0, 0]);
        value_or_error(error, value)
            .map(|_| ())
            .map_err(InstructionFenceError)
    })
}

fn for_each_hardware_mask<E>(
    mut cpus: crate::cpu::CpuSet,
    mut operation: impl FnMut(usize, usize) -> Result<(), E>,
) -> Result<(), E> {
    while let Some(first) = cpus.iter().next() {
        let first_hardware = crate::cpu::hardware_id(first).raw();
        let base = first_hardware / usize::BITS as usize * usize::BITS as usize;
        let mut mask = 0usize;
        for cpu in cpus.iter() {
            let hardware = crate::cpu::hardware_id(cpu).raw();
            if hardware >= base && hardware - base < usize::BITS as usize {
                mask |= 1usize << (hardware - base);
                cpus.remove(cpu);
            }
        }
        operation(mask, base)?;
    }
    Ok(())
}

/// @description 请求 SBI 重置或关闭整个系统。
///
/// @param reset_type SBI SRST reset type。
/// @param reset_reason SBI SRST reset reason。
/// @return 正常成功不会返回；firmware 返回时以 `Ok(())` 或 SBI error 表示结果。
pub(crate) fn reset_system(reset_type: usize, reset_reason: usize) -> Result<(), ResetError> {
    let (error, value) = sbi_call(
        EID_SYSTEM_RESET,
        FID_SYSTEM_RESET,
        [reset_type, reset_reason, 0, 0, 0, 0],
    );
    value_or_error(error, value).map(|_| ()).map_err(ResetError)
}
