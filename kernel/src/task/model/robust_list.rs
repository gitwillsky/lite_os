use super::{AddressSpace, TaskControlBlock};
use crate::memory::UserFaultLimits;

const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
// futex word 的高两位归 WAITERS/OWNER_DIED，实际 local PID limit 小于此 mask。
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const ROBUST_LIST_LIMIT: usize = 2048;

/// 稳定 robust registration 查询的目标、授权和可取消 exec-guard 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RobustListError {
    NotFound,
    Permission,
    OutOfMemory,
    Interrupted,
}

/// 原生 robust link 的地址与 PI tag；tag 属于入链指针，不是 futex 地址或下一 node 的属性。
#[derive(Clone, Copy)]
struct RobustLink {
    address: usize,
    pi: bool,
}
impl RobustLink {
    fn decode(pointer: usize) -> Self {
        Self {
            address: pointer & !1,
            pi: pointer & 1 != 0,
        }
    }
}

impl TaskControlBlock {
    /// 旧 mm 的稳定 key 在预置 waiter 的 guard 内取得，实际 wake 在 guard 归还后执行。
    fn wake_robust_waiter(&self, mm: &AddressSpace, address: usize, limits: UserFaultLimits) {
        let mut wait = self.take_memory_cleanup_wait();
        let _ = crate::task::futex_wake_with_key(1, u32::MAX, &mut wait, |wait, consume| {
            let mut memory = mm.memory_set.lock_prepared(wait);
            let key = memory.futex_key(address, core::ptr::from_ref(mm).addr(), false, limits)?;
            consume(key);
            Ok(())
        });
        self.restore_memory_cleanup_wait(wait);
    }

    pub(crate) fn set_robust_list(&self, head: usize, length: usize) -> Result<(), ()> {
        if length != 3 * core::mem::size_of::<usize>() {
            return Err(());
        }
        *self.thread().robust_list.lock() = (head != 0).then_some(head);
        Ok(())
    }

    /// 在目标的同一 exec credential owner 下授权并复制 registration scalar。
    /// 不访问目标用户地址；返回的 head 只是 native ABI 的地址值，不保活旧 mm。
    pub(in crate::task) fn robust_list_head_for(
        &self,
        caller: &TaskControlBlock,
    ) -> Result<usize, RobustListError> {
        if self.is_kernel_thread() {
            return if caller.may_inspect_thread_real(self) {
                Ok(0)
            } else {
                Err(RobustListError::Permission)
            };
        }
        let _exec = self
            .credential_guard()
            .lock_killable()
            .map_err(|error| match error {
                crate::sync::TaskMutexLockError::OutOfMemory => RobustListError::OutOfMemory,
                crate::sync::TaskMutexLockError::Interrupted => RobustListError::Interrupted,
            })?;
        if !caller.may_inspect_thread_real(self) {
            return Err(RobustListError::Permission);
        }
        Ok(self.thread().robust_list.lock().unwrap_or(0))
    }

    /// 发布原生 robust futex 的 owner-death；PI handoff 由实际 PI state exit owner 执行。
    ///
    /// # Parameters
    ///
    /// - `entry`: 已剥离 native PI tag 的 robust-list node。
    /// - `offset`: node 到 futex word 的 signed byte offset。
    /// - `pending_operation`: true 表示 `list_op_pending` 的 unlock/acquire 窗口。
    ///
    /// # Returns
    ///
    /// 完成或 owner 已转移为 Ok；地址、读取或 CAS fault 为 Err 并终止本次 traversal。
    fn mark_robust_futex_dead(
        &self,
        address_space: &AddressSpace,
        fault_limits: UserFaultLimits,
        entry: RobustLink,
        offset: isize,
        pending_operation: bool,
    ) -> Result<(), ()> {
        let Some(address) = entry.address.checked_add_signed(offset) else {
            return Err(());
        };
        if address & (core::mem::size_of::<u32>() - 1) != 0 {
            return Err(());
        }
        let mut observed = self
            .with_cleanup_memory(address_space, |memory| {
                memory.load_user_u32(address, fault_limits)
            })
            .map_err(|_| ())?;
        loop {
            let owner = observed & FUTEX_TID_MASK;
            // 普通 pending unlock 可在 word 清零后、实际 wake 前退出；不伪造 OWNER_DIED。
            // PI 必须由实际 ownership graph 交接，不能用普通 FUTEX_WAKE 消费它的 waiter。
            if pending_operation && !entry.pi && owner == 0 {
                self.wake_robust_waiter(address_space, address, fault_limits);
                return Ok(());
            }
            if owner != self.visible_tid() {
                return Ok(());
            }
            let replacement = observed & FUTEX_WAITERS | FUTEX_OWNER_DIED;
            let exchange = self.with_cleanup_memory(address_space, |memory| {
                memory.compare_exchange_user_u32(address, observed, replacement, fault_limits)
            });
            match exchange {
                Err(_) => return Err(()),
                Ok(Err(conflict)) => observed = conflict,
                Ok(Ok(replaced)) => {
                    debug_assert_eq!(replaced, observed);
                    if !entry.pi && replaced & FUTEX_WAITERS != 0 {
                        self.wake_robust_waiter(address_space, address, fault_limits);
                    }
                    return Ok(());
                }
            }
        }
    }

    pub(in crate::task) fn cleanup_robust_list(&self) {
        let Some(head) = self.thread().robust_list.lock().take() else {
            return;
        };
        let fault_limits = self.user_fault_limits();
        let address_space = self.address_space();
        let mut header = [0u8; 3 * core::mem::size_of::<usize>()];
        if self
            .with_cleanup_memory(&address_space, |memory| {
                memory.copy_from_user(head, &mut header, fault_limits)
            })
            .is_err()
        {
            return;
        }
        let word = core::mem::size_of::<usize>();
        let mut entry =
            RobustLink::decode(usize::from_ne_bytes(header[0..word].try_into().unwrap()));
        let offset = isize::from_ne_bytes(header[word..2 * word].try_into().unwrap());
        let pending = RobustLink::decode(usize::from_ne_bytes(
            header[2 * word..3 * word].try_into().unwrap(),
        ));
        for _ in 0..ROBUST_LIST_LIMIT {
            if entry.address == head {
                break;
            }
            if entry.address == 0 {
                return;
            }
            let mut next = [0u8; core::mem::size_of::<usize>()];
            let next_valid = self
                .with_cleanup_memory(&address_space, |memory| {
                    memory.copy_from_user(entry.address, &mut next, fault_limits)
                })
                .is_ok();
            if entry.address != pending.address
                && self
                    .mark_robust_futex_dead(&address_space, fault_limits, entry, offset, false)
                    .is_err()
            {
                return;
            }
            if !next_valid {
                return;
            }
            entry = RobustLink::decode(usize::from_ne_bytes(next));
        }
        if pending.address != 0 {
            let _ =
                self.mark_robust_futex_dead(&address_space, fault_limits, pending, offset, true);
        }
    }
}

/// 查询时取得的实际退休 leader aliases；并发 exec 重指 Process 不改变所查询的旧 Thread。
pub(in crate::task) struct RetiredThreadInspection {
    process: alloc::sync::Arc<super::Process>,
    credentials: alloc::sync::Arc<spin::Mutex<super::Credentials>>,
    mm: super::MmHandle,
}
impl super::Process {
    /// graph lookup 已确定 leader 退出；只 pin 其真实 scalar handles，不保活 TCB 或 MM root。
    pub(in crate::task) fn retired_inspection_target(
        self: &alloc::sync::Arc<Self>,
    ) -> RetiredThreadInspection {
        RetiredThreadInspection {
            process: self.clone(),
            credentials: self.leader_credentials.lock().clone(),
            mm: self.leader_mm.lock().clone(),
        }
    }
}
impl RetiredThreadInspection {
    /// robust registration 在 leader 退出时已清除；仍按所 pin 目标的退出授权事实查询零 head。
    pub(in crate::task) fn head_for(
        &self,
        caller: &TaskControlBlock,
    ) -> Result<usize, RobustListError> {
        self.authorize(caller)?;
        Ok(0)
    }
    /// 查询时 pin 的已退休 Thread 没有文件表；exec 不能把它变成新 leader 的资源查询。
    pub(in crate::task) fn authorize(
        &self,
        caller: &TaskControlBlock,
    ) -> Result<(), RobustListError> {
        let _exec = self
            .process
            .credential_guard
            .lock_killable()
            .map_err(|error| match error {
                crate::sync::TaskMutexLockError::OutOfMemory => RobustListError::OutOfMemory,
                crate::sync::TaskMutexLockError::Interrupted => RobustListError::Interrupted,
            })?;
        let credentials = self.credentials.lock().snapshot();
        let mm = self.mm.lock().clone();
        if !caller.may_inspect_real_identity(self.process.tgid.key(), credentials, mm) {
            return Err(RobustListError::Permission);
        }
        Ok(())
    }
}
