use super::{AddressSpace, TaskControlBlock};
use crate::{memory::UserFaultLimits, task::pid::PID_MAX};

const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = PID_MAX as u32;
const ROBUST_LIST_LIMIT: usize = 2048;

impl AddressSpace {
    /// @description 在一次 mm-lock 临界区内原子替换 robust futex word。
    fn compare_exchange_robust_word(
        &self,
        address: usize,
        current: u32,
        replacement: u32,
        fault_limits: UserFaultLimits,
    ) -> Result<Result<u32, u32>, crate::memory::UserAccessError> {
        self.memory_set.lock().compare_exchange_user_u32(
            address,
            current,
            replacement,
            fault_limits,
        )
    }

    /// @description 用本次 cleanup 的 old-mm/limits snapshot 解析并唤醒一个 robust waiter。
    fn wake_robust_waiter(&self, address: usize, fault_limits: UserFaultLimits) {
        let _ = crate::task::futex_wake_with_key(1, u32::MAX, |consume| {
            self.with_futex_key(address, false, fault_limits, consume)
        });
    }
}

impl TaskControlBlock {
    pub(crate) fn set_robust_list(&self, head: usize, length: usize) -> Result<(), ()> {
        if length != 3 * core::mem::size_of::<usize>() {
            return Err(());
        }
        *self.thread.robust_list.lock() = (head != 0).then_some(head);
        Ok(())
    }

    /// @description 发布一个 non-PI robust futex 的 owner-death 状态。
    /// @param entry robust-list node 地址。
    /// @param offset node 到 futex word 的 signed byte offset。
    /// @param pending_operation true 表示 `list_op_pending` 的 unlock/acquire 窗口。
    /// @return 完成或 owner 已转移为 Ok；地址、读取或 CAS fault 为 Err 并终止本次 traversal。
    fn mark_robust_futex_dead(
        &self,
        address_space: &AddressSpace,
        fault_limits: UserFaultLimits,
        entry: usize,
        offset: isize,
        pending_operation: bool,
    ) -> Result<(), ()> {
        let Some(address) = entry.checked_add_signed(offset) else {
            return Err(());
        };
        if address & (core::mem::size_of::<u32>() - 1) != 0 {
            return Err(());
        }
        let mut bytes = [0u8; 4];
        if address_space
            .copy_from_user(address, &mut bytes, fault_limits)
            .is_err()
        {
            return Err(());
        }
        let mut observed = u32::from_ne_bytes(bytes);
        loop {
            let owner = observed & FUTEX_TID_MASK;
            // A non-PI pending operation can be killed after userspace clears
            // ownership but before its wake syscall. The word is already
            // consistent, so wake without manufacturing OWNER_DIED.
            if pending_operation && owner == 0 {
                address_space.wake_robust_waiter(address, fault_limits);
                return Ok(());
            }
            if owner != self.tid() as u32 {
                return Ok(());
            }
            let replacement = observed & FUTEX_WAITERS | FUTEX_OWNER_DIED;
            let exchange = address_space.compare_exchange_robust_word(
                address,
                observed,
                replacement,
                fault_limits,
            );
            match exchange {
                Err(_) => return Err(()),
                Ok(Err(conflict)) => observed = conflict,
                Ok(Ok(replaced)) => {
                    debug_assert_eq!(replaced, observed);
                    if replaced & FUTEX_WAITERS != 0 {
                        address_space.wake_robust_waiter(address, fault_limits);
                    }
                    return Ok(());
                }
            }
        }
    }

    pub(in crate::task) fn cleanup_robust_list(&self) {
        let Some(head) = self.thread.robust_list.lock().take() else {
            return;
        };
        let fault_limits = self.user_fault_limits();
        let address_space = self.process.address_space();
        let mut header = [0u8; 3 * core::mem::size_of::<usize>()];
        if address_space
            .copy_from_user(head, &mut header, fault_limits)
            .is_err()
        {
            return;
        }
        let word = core::mem::size_of::<usize>();
        let mut entry = usize::from_ne_bytes(header[0..word].try_into().unwrap());
        let offset = isize::from_ne_bytes(header[word..2 * word].try_into().unwrap());
        let pending = usize::from_ne_bytes(header[2 * word..3 * word].try_into().unwrap());
        for _ in 0..ROBUST_LIST_LIMIT {
            if entry == head {
                break;
            }
            if entry == 0 {
                return;
            }
            let mut next = [0u8; core::mem::size_of::<usize>()];
            let next_valid = address_space
                .copy_from_user(entry, &mut next, fault_limits)
                .is_ok();
            if entry != pending
                && self
                    .mark_robust_futex_dead(&address_space, fault_limits, entry, offset, false)
                    .is_err()
            {
                return;
            }
            if !next_valid {
                return;
            }
            entry = usize::from_ne_bytes(next);
        }
        if pending != 0 {
            let _ =
                self.mark_robust_futex_dead(&address_space, fault_limits, pending, offset, true);
        }
    }
}
