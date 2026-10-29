//! 标准设备命令的异步驱动：协议属于 DeviceFile，用户页 fault 属于阻塞车道。
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use spin::Mutex;
use syscall_abi::errno;

use super::{
    exec::{Outcome, run_lane},
    lane,
    wait::Waiter,
};
use crate::{
    execution::task::TaskControlBlock,
    file::{OpenFileDescription, OpenFileKind},
    fs::chardev::{PendingCommand, UringCommand},
    syscall::device::TaskUserMemory,
};

/// 车道与 reactor 间的唯一令牌交接槽。锁内只移动值，不执行设备方法或用户复制。
/// 缺失时只能让 reactor 自己 fault 用户页，或为一次性返回值引入另一套 worker。
struct Progress {
    command: Option<Box<dyn PendingCommand>>,
    result: Option<isize>,
}

/// 驱动已解析 OFD 的设备命令；等待设备就绪不占用阻塞 worker。
///
/// # Parameters
/// - `ofd`: 实际设备 OFD，完成/取消前保活；正在复制的车道另持引用。
/// - `request`: prep 复制的标准命令输入。
/// - `task`: 提交线程的 Weak，车道执行时才取得短暂强引用。
/// - `waiter`: 同一链的取消与通知 owner。
///
/// # Returns
/// 设备实际 CQE 结果；OOM 为 ENOMEM，执行前取消为 ECANCELED，已登记命令由设备决定取消 errno。
pub(super) async fn drive(
    ofd: Arc<OpenFileDescription>,
    request: UringCommand,
    task: &Weak<TaskControlBlock>,
    waiter: &Waiter,
) -> Outcome {
    let Ok(progress) = Arc::try_new(Mutex::new(Progress {
        command: None,
        result: None,
    })) else {
        return Outcome::of(-errno::ENOMEM);
    };
    // 1. REGISTER 的 iovec 导入、COMMIT 的回复复制都可能 fault，只在车道内启动设备命令。
    let slot = progress.clone();
    let target = ofd.clone();
    let work: lane::Work = match Box::try_new(move |task: &TaskControlBlock| {
        let OpenFileKind::Device(device) = &target.kind else {
            unreachable!("device OFD checked at issue")
        };
        let result = device.uring_command(&request, &TaskUserMemory(task));
        let mut slot = slot.lock();
        match result {
            Ok(command) => slot.command = Some(command),
            Err(error) => slot.result = Some(-error.errno()),
        }
        0
    }) {
        Ok(work) => work,
        Err(_) => return Outcome::of(-errno::ENOMEM),
    };
    let started = run_lane(work, None, task, waiter).await;
    if started.res < 0 {
        return started;
    }
    let notify = waiter.notifier();
    loop {
        let (command, result) = {
            let mut progress = progress.lock();
            (progress.command.take(), progress.result.take())
        };
        if let Some(result) = result {
            return Outcome::of(result);
        }
        let mut command = command.expect("successful lane returned command ownership");
        if waiter.take_canceled() || waiter.link_timed_out() || task.upgrade().is_none() {
            return Outcome::of(command.canceled_result());
        }
        // 2. 清通知后原子登记/检查，ready 不访问用户页；Pending 时归还令牌并让出 reactor。
        waiter.clear_notified();
        if !command.ready(&notify) {
            progress.lock().command = Some(command);
            waiter.block().await;
            continue;
        }
        // 3. 有请求时才派发一次复制；车道结束后还原可取消状态，再按同一通知协议等待下一次。
        let slot = progress.clone();
        let target = ofd.clone();
        let notify = notify.clone();
        let work: lane::Work = match Box::try_new(move |task: &TaskControlBlock| {
            let _target = target;
            let result = command
                .step(&TaskUserMemory(task), &notify)
                .map(|result| result.unwrap_or_else(|error| -error.errno()));
            let mut slot = slot.lock();
            slot.command = Some(command);
            slot.result = result;
            0
        }) {
            Ok(work) => work,
            Err(_) => return Outcome::of(-errno::ENOMEM),
        };
        let copied = run_lane(work, None, task, waiter).await;
        if copied.res < 0 {
            return copied;
        }
    }
}
