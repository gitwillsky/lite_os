use spin::Mutex;

const EID_TIME: usize = 0x5449_4d45;
const EID_IPI: usize = 0x0073_5049;
const EID_RFENCE: usize = 0x5246_4e43;
const EID_SYSTEM_RESET: usize = 0x5352_5354;
const EID_DEBUG_CONSOLE: usize = 0x4442_434e;
const EID_BASE: usize = 0x10;

const FID_SET_TIMER: usize = 0;
const FID_SEND_IPI: usize = 0;
const FID_REMOTE_SFENCE_VMA: usize = 1;
const FID_SYSTEM_RESET: usize = 0;
const FID_CONSOLE_READ: usize = 1;
const FID_CONSOLE_WRITE_BYTE: usize = 2;
const FID_PROBE_EXTENSION: usize = 3;
const SBI_ERR_FAILED: isize = -1;

static CONSOLE_INPUT_BYTE: Mutex<u8> = Mutex::new(0);

/// @description 执行 SBI v0.2+ EID/FID 调用。
///
/// @param eid SBI extension ID，写入 `a7`。
/// @param fid SBI function ID，写入 `a6`。
/// @param args 六个 XLEN 参数，依次写入 `a0..a5`。
/// @return `(error, value)`，分别来自 `a0` 和 `a1`。
#[inline(always)]
pub fn sbi_call(eid: usize, fid: usize, args: [usize; 6]) -> (isize, usize) {
    let error: isize;
    let value: usize;
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
fn value_or_error(error: isize, value: usize) -> Result<usize, isize> {
    if error == 0 { Ok(value) } else { Err(error) }
}

fn probe_extension(eid: usize) -> Result<bool, isize> {
    let (error, value) = sbi_call(EID_BASE, FID_PROBE_EXTENSION, [eid, 0, 0, 0, 0, 0]);
    value_or_error(error, value).map(|value| value != 0)
}

/// @description 验证 kernel 启动与 fail-stop 路径依赖的 SBI extension。
///
/// @return 全部 extension 可用时返回；缺失或 probe 失败触发 kernel panic。
pub fn verify_required_extensions() {
    for (eid, name) in [
        (EID_TIME, "TIME"),
        (EID_IPI, "IPI"),
        (EID_RFENCE, "RFENCE"),
        (EID_SYSTEM_RESET, "SRST"),
        (EID_DEBUG_CONSOLE, "DBCN"),
    ] {
        assert!(
            probe_extension(eid).unwrap_or(false),
            "required SBI extension {} ({:#x}) is unavailable",
            name,
            eid
        );
    }
}

/// @description 通过 SBI DBCN 写出单字节，不使用 legacy console extension。
///
/// @param byte 待写出的字节。
/// @return 成功返回 `Ok(())`；firmware 拒绝或不支持时返回 SBI error。
pub fn console_putchar(byte: u8) -> Result<(), isize> {
    let (error, value) = sbi_call(
        EID_DEBUG_CONSOLE,
        FID_CONSOLE_WRITE_BYTE,
        [byte as usize, 0, 0, 0, 0, 0],
    );
    value_or_error(error, value).map(|_| ())
}

/// @description 通过 SBI DBCN 非阻塞读取一个字节。
///
/// @return `Ok(Some(byte))` 表示读到字节，`Ok(None)` 表示当前无输入；失败返回 SBI error。
pub fn console_getchar() -> Result<Option<u8>, isize> {
    let mut byte = CONSOLE_INPUT_BYTE.lock();
    let physical_address = (&mut *byte as *mut u8) as usize;
    let (error, count) = sbi_call(
        EID_DEBUG_CONSOLE,
        FID_CONSOLE_READ,
        [1, physical_address, 0, 0, 0, 0],
    );
    match value_or_error(error, count)? {
        0 => Ok(None),
        1 => Ok(Some(*byte)),
        _ => Err(SBI_ERR_FAILED),
    }
}

/// @description 通过 SBI TIME 设置当前 hart 的绝对 timer deadline。
///
/// @param timer_value `time` CSR 同一计数域中的绝对值。
/// @return 成功返回 `Ok(())`，失败返回 SBI error。
pub fn set_timer(timer_value: u64) -> Result<(), isize> {
    let (error, value) = sbi_call(
        EID_TIME,
        FID_SET_TIMER,
        [timer_value as usize, 0, 0, 0, 0, 0],
    );
    value_or_error(error, value).map(|_| ())
}

/// @description 通过 SBI IPI 向 hart mask 发送 supervisor software interrupt。
///
/// @param hart_mask 从 `hart_mask_base` 开始的 hart 位图。
/// @param hart_mask_base 位图 bit 0 对应的 hart ID。
/// @return 成功返回 `Ok(())`，失败返回 SBI error。
pub fn sbi_send_ipi(hart_mask: usize, hart_mask_base: usize) -> Result<(), isize> {
    let (error, value) = sbi_call(
        EID_IPI,
        FID_SEND_IPI,
        [hart_mask, hart_mask_base, 0, 0, 0, 0],
    );
    value_or_error(error, value).map(|_| ())
}

/// @description 请求目标 hart 同步完成 `SFENCE.VMA`。
///
/// @param hart_mask 从 `hart_mask_base` 开始的 hart 位图。
/// @param hart_mask_base 位图 bit 0 对应的 hart ID。
/// @param start_address 刷新区间起始虚拟地址；与 `size` 同为零表示全局刷新。
/// @param size 刷新区间字节数；与 `start_address` 同为零表示全局刷新。
/// @return SBI 仅在所有目标 hart 完成 fence 后返回 `Ok(())`；失败返回 SBI error。
pub fn remote_sfence_vma(
    hart_mask: usize,
    hart_mask_base: usize,
    start_address: usize,
    size: usize,
) -> Result<(), isize> {
    let (error, value) = sbi_call(
        EID_RFENCE,
        FID_REMOTE_SFENCE_VMA,
        [hart_mask, hart_mask_base, start_address, size, 0, 0],
    );
    value_or_error(error, value).map(|_| ())
}

/// @description 请求 SBI 重置或关闭整个系统。
///
/// @param reset_type SBI SRST reset type。
/// @param reset_reason SBI SRST reset reason。
/// @return 正常成功不会返回；firmware 返回时以 `Ok(())` 或 SBI error 表示结果。
pub fn system_reset(reset_type: usize, reset_reason: usize) -> Result<(), isize> {
    let (error, value) = sbi_call(
        EID_SYSTEM_RESET,
        FID_SYSTEM_RESET,
        [reset_type, reset_reason, 0, 0, 0, 0],
    );
    value_or_error(error, value).map(|_| ())
}
