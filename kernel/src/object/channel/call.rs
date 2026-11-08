//! 每个原生线程复用一份 call waiter；在途对象先从槽位移出，等待期间不持槽位锁。
use super::*;
use core::ops::Deref;

/// 原生线程独占的空闲调用存储。进程/执行体领域不拥有 channel 事务或回复状态。
pub(in crate::object) struct CallSlot {
    // OWNER: 仅保留未登记且无回复的唯一 Arc；与 Thread 同寿命，不建立全局目录或每次调用分配。
    idle: Mutex<Option<Arc<CallWaiter>>>,
}
impl CallSlot {
    pub(in crate::object) const fn new() -> Self {
        Self {
            idle: Mutex::new(None),
        }
    }
    pub(super) fn acquire(&self) -> Result<CallLease<'_>, Status> {
        let idle = self.idle.lock().take();
        let mut waiter = match idle {
            Some(waiter) => waiter,
            None => Arc::try_new(CallWaiter::new(0)).map_err(|_| native_abi::ERR_NO_MEMORY)?,
        };
        // 前一次已在原 channel 锁内撤销 membership；任何回复/关闭通知都已结束，才会归还唯一 Arc。
        // 新建 TaskEvent 同时清除上次 completion-before-wait 留下的 pending，旧 ticket 永不复用。
        *Arc::get_mut(&mut waiter).expect("idle call waiter still referenced") = CallWaiter::new(0);
        Ok(CallLease {
            slot: self,
            waiter: Some(waiter),
        })
    }
    /// 执行体退出时放弃空闲存储；即使用户仍保留已退出 Thread 句柄，也不保留历史调用容量。
    pub(in crate::object) fn retire(&self) {
        let retired = self.idle.lock().take();
        drop(retired);
    }
}
/// 在途调用租约。调用从所有端点登记移出、回复取走且锁释放后，析构才归还线程空闲槽。
pub(super) struct CallLease<'a> {
    slot: &'a CallSlot,
    waiter: Option<Arc<CallWaiter>>,
}
impl Deref for CallLease<'_> {
    type Target = Arc<CallWaiter>;
    fn deref(&self) -> &Self::Target {
        self.waiter.as_ref().expect("live call lease")
    }
}
impl CallLease<'_> {
    pub(super) fn set_txid(&mut self, txid: u32) {
        Arc::get_mut(self.waiter.as_mut().expect("live call lease"))
            .expect("call waiter published before assigning txid")
            .txid = txid;
    }
}
impl ChannelState {
    pub(super) fn next_txid(&mut self) -> Result<u32, Status> {
        let mut candidate = self.next_txid.wrapping_add(1) & !KERNEL_TXID_BIT;
        if candidate > self.next_txid && candidate < self.free_until {
            self.next_txid = candidate;
            return Ok(candidate | KERNEL_TXID_BIT);
        }
        let registered = self
            .sides
            .iter()
            .map(|side| side.calls.len())
            .sum::<usize>();
        if registered >= KERNEL_TXID_BIT as usize {
            return Err(native_abi::ERR_NO_RESOURCES);
        }
        // 1. 环绕/旧边界才扫描；普通分配沿已证明的空隙推进，仍为 O(1)。
        // 2. 新登记的 ID <= 当前游标，移除登记不缩小空隙，故此前证明不会被后续调用破坏。
        // 3. 连续 registered + 1 个不同候选中必有空位，不能在锁内无界自旋。
        for _ in 0..=registered {
            let mut occupied = false;
            let mut free_until = KERNEL_TXID_BIT;
            for call in self.sides.iter().flat_map(|side| &side.calls) {
                let low = call.txid & !KERNEL_TXID_BIT;
                if low == candidate {
                    occupied = true;
                    break;
                }
                if low > candidate {
                    free_until = free_until.min(low);
                }
            }
            if !occupied {
                self.next_txid = candidate;
                self.free_until = free_until;
                return Ok(candidate | KERNEL_TXID_BIT);
            }
            candidate = candidate.wrapping_add(1) & !KERNEL_TXID_BIT;
        }
        unreachable!("call txid namespace accounting corrupted")
    }
}
impl Drop for CallLease<'_> {
    fn drop(&mut self) {
        let mut waiter = self.waiter.take().expect("live call lease");
        let unique =
            Arc::get_mut(&mut waiter).expect("registered call waiter returned to idle pool");
        assert!(unique.reply.get_mut().is_none(), "unconsumed call reply");
        let mut idle = self.slot.idle.lock();
        assert!(idle.is_none(), "one native thread issued overlapping calls");
        *idle = Some(waiter);
    }
}
impl CallWaiter {
    fn new(txid: u32) -> Self {
        Self {
            txid,
            event: TaskEvent::new(),
            reply: Mutex::new(None),
        }
    }
}
