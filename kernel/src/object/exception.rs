//! 异常：未在 restricted mode 中发生、且地址空间无法解决的同步异常交给异常 channel 处理。
//!
//! 1. 内核依次查找线程所在进程、其 Job 及祖先 Job 的异常 channel，向第一个存活的写入一条
//!    [`native_abi::ExceptionReport`] 消息，消息携带一个 [`Exception`] 句柄；
//! 2. 出错线程经 [`ExceptionWaiter`] 阻塞，直到处理者 `exception_resolve` 或关闭该句柄；
//! 3. `Resume` 让线程从（可能被修改的）寄存器继续；`TryNext`（含关闭句柄未处理）交给下一级；
//!    没有任何处理者时终止进程。
//!
//! 句柄侧（[`Exception`]）与等待侧（[`ExceptionWaiter`]）分开持有共享状态：处理者关闭最后一个句柄时
//! `Exception` 析构并给出 `TryNext`，等待者因此一定能醒来。

use alloc::sync::Arc;

use native_abi::register::RegisterState;
use native_abi::{ObjectType, Status, rights};
use spin::Mutex;

use super::{KernelObject, next_koid};
use crate::execution::sync::{WaitQueue, WaitResult};

/// 处理者给出的结论。
#[derive(Clone)]
pub(crate) enum ExceptionResolution {
    /// 从给定寄存器（`None` 表示不变）继续执行。寄存器装箱：结论只在冷路径构造。
    Resume(Option<alloc::boxed::Box<RegisterState>>),
    /// 交给下一级处理者。
    TryNext,
}

struct Shared {
    // OWNER: 唯一结论；第一次写入生效。
    resolution: Mutex<Option<ExceptionResolution>>,
    resolved: WaitQueue,
}

impl Shared {
    fn resolve(&self, resolution: ExceptionResolution) {
        {
            let mut slot = self.resolution.lock();
            if slot.is_some() {
                return;
            }
            *slot = Some(resolution);
        }
        self.resolved.wake_all();
    }
}

/// 交给处理者的异常句柄对象。
pub(crate) struct Exception {
    koid: u64,
    shared: Arc<Shared>,
}

/// 出错线程持有的等待端。
pub(crate) struct ExceptionWaiter {
    shared: Arc<Shared>,
}

impl Exception {
    /// 为一次异常创建句柄对象与等待端；异常报告随同一条 channel 消息交给处理者。
    ///
    /// # Errors
    /// 分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn create() -> Result<(Arc<Self>, ExceptionWaiter), Status> {
        let shared = Arc::try_new(Shared {
            resolution: Mutex::new(None),
            resolved: WaitQueue::new(),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let exception = Arc::try_new(Self {
            koid: next_koid(),
            shared: shared.clone(),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Ok((exception, ExceptionWaiter { shared }))
    }

    /// 处理者给出结论；只接受第一次。
    pub(crate) fn resolve(&self, resolution: ExceptionResolution) {
        self.shared.resolve(resolution);
    }
}

impl Drop for Exception {
    fn drop(&mut self) {
        // 关闭句柄而未给出结论，等同于交给下一级。
        self.shared.resolve(ExceptionResolution::TryNext);
    }
}

impl ExceptionWaiter {
    /// 等待结论；线程被终止或分配失败时返回 `None`。
    pub(crate) fn wait(&self) -> Option<ExceptionResolution> {
        match self
            .shared
            .resolved
            .wait_until(|| self.shared.resolution.lock().is_some(), None)
        {
            WaitResult::Woken => self.shared.resolution.lock().clone(),
            WaitResult::TimedOut | WaitResult::Interrupted | WaitResult::OutOfMemory => None,
        }
    }
}

impl KernelObject for Exception {
    fn object_type(&self) -> ObjectType {
        ObjectType::Exception
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::TRANSFER | rights::READ | rights::WRITE | rights::INSPECT
    }
}
