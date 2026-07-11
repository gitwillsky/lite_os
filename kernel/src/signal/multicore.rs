//! @description 为正在其他 hart 上执行的 task 发送信号检查 IPI。
//!
//! 信号内容和 pending 状态只存于 `TaskControlBlock::signal_state`；本模块不维护第二套
//! 消息队列或 PID map。`CURRENT_PID` 只是定位运行 hart 的瞬时 hint，不拥有 task 状态。

use core::sync::atomic::{AtomicUsize, Ordering};

use super::core::SignalError;
use crate::arch::hart::{MAX_CORES, hart_id};

static CURRENT_PID: [AtomicUsize; MAX_CORES] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// @description 发布指定 hart 当前运行的 PID hint。
///
/// @param core_id owner hart ID。
/// @param pid 当前运行 task 的 PID。
/// @return 无返回值。
/// @errors core_id 越界表示 scheduler 不变量破坏并触发 panic。
pub fn update_task_on_core(core_id: usize, pid: usize) {
    let slot = CURRENT_PID
        .get(core_id)
        .expect("signal current-PID hart index out of range");
    // Release 让远端在决定发送 IPI 前看到 switch_to_task 已发布的 PID；它不发布 TCB 内部状态。
    slot.store(pid, Ordering::Release);
}

/// @description 仅当指定 PID 仍是该 hart 的 current task 时清除 hint。
///
/// @param core_id owner hart ID。
/// @param pid 预期正在退出 CPU 的 PID。
/// @return 无返回值。
/// @errors core_id 越界或 PID 不匹配表示 scheduler 状态转换错误并触发 panic。
pub fn clear_task_on_core(core_id: usize, pid: usize) {
    let slot = CURRENT_PID
        .get(core_id)
        .expect("signal current-PID hart index out of range");
    slot.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire)
        .expect("signal current-PID does not match task leaving CPU");
}

/// @description 若目标 PID 正在某个远端 hart 执行，则发送 IPI 促使其检查 TCB pending bitmap。
///
/// @param pid 已写入 pending signal 的目标 PID。
/// @return 找到远端并成功发送或目标当前未运行时返回 `Ok(())`。
/// @errors SBI IPI 失败返回 `SignalError::InternalError`。
pub fn notify_running_task(pid: usize) -> Result<(), SignalError> {
    let current = hart_id();
    for (target, slot) in CURRENT_PID.iter().enumerate() {
        // Acquire 与 switch_to_task 的 Release 配对；过期 hint 最多产生一次无害 IPI。
        if slot.load(Ordering::Acquire) != pid || target == current {
            continue;
        }
        return crate::arch::sbi::sbi_send_ipi(1usize << target, 0)
            .map_err(|_| SignalError::InternalError);
    }
    Ok(())
}
