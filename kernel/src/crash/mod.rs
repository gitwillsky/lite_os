//! 内核崩溃处理：现场保存、蓝屏与软看门狗。
//!
//! # 分工
//!
//! - [`record`]：RAM 顶端保留的崩溃记录区。panic 把文本现场写进去，warm reset 之后下一次启动读回，
//!   通过 `/proc/last_panic` 与启动日志呈现。
//! - [`panic`]：panic 流程的唯一 owner——争夺所有权、IPI 停住其余 CPU、写记录、画蓝屏、按策略停机/重启。
//! - `bluescreen`：Windows 2000 风格的蓝屏，画到 adapter 为 panic 专设的 scanout resource。
//! - `watchdog`：boot CPU 上 deferred work 停滞的软检测，把“静默冻结”变成带现场的 panic。
//!
//! # 局限
//!
//! RAM 内记录只跨 warm reset（PSCI/SBI reset、`panic=N` 自动重启、QEMU `system_reset`）存活；
//! 宿主关闭并重启整个虚拟机进程会丢失 RAM，此时现场只剩串口输出与屏幕上的蓝屏。

mod backtrace;
mod bluescreen;
mod font;
mod panic;
mod record;
mod watchdog;

use alloc::vec::Vec;

use spin::Once;

pub(crate) use panic::{
    configure, handle as handle_panic, initialize, register_display, stop_if_requested,
};
pub(crate) use record::reserve as reserve_region;
pub(crate) use watchdog::{check_tick as watchdog_tick, note_deferred_progress};

// OWNER: boot CPU 在启动期把上一次启动留下的崩溃记录读出并保存一次，之后只读；`/proc/last_panic`
// 直接引用它。缺少这份拷贝，记录区在读取后即被作废，用户只能在启动日志里匆匆看一眼。
static PREVIOUS: Once<Vec<u8>> = Once::new();

/// 读回上一次启动留下的崩溃记录并作废记录区；必须在 `memory::init` 之后、heap 可用时调用。
pub(crate) fn recover() {
    let Some(dump) = record::recover() else {
        return;
    };
    warn!(
        "previous boot ended in a kernel panic; crash record ({} bytes) is available in /proc/last_panic",
        dump.len()
    );
    PREVIOUS.call_once(|| dump);
}

/// 上一次启动的崩溃记录文本；上次正常结束（或记录区无效）时为 `None`。
pub(crate) fn previous_dump() -> Option<&'static [u8]> {
    PREVIOUS.get().map(Vec::as_slice)
}
