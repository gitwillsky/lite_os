use core::ops::{Deref, DerefMut};

use spin::{Mutex, Once};

use super::{InetEndpoint, NetworkStack, SOCKET_STORAGE_CAPACITY};
use crate::{
    socket::SocketError,
    sync::{TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation},
};

#[path = "protocol_owner/pending_cleanup.rs"]
mod pending_cleanup;
use pending_cleanup::PendingCleanup;

const NETWORK_CLEANUP_BUDGET: usize = 64;

struct NetworkStackState {
    stack: NetworkStack,
    // OWNER: 每个成功 take 在 placeholder 可见前后精确 +1/-1；缺失时 poll 会把 closed
    // placeholder 当真实 endpoint 推进并丢失 ingress/egress protocol state。
    payload_loans: usize,
}

/// 唯一 NetworkStack 的可睡眠 task-context guard。
pub(super) struct NetworkStackGuard<'a> {
    state: TaskMutexGuard<'a, NetworkStackState>,
    // 本 guard 取得 owner 后固定的 bounded-drain 结果；不复制持久 cleanup state。
    cleanup_backlog: bool,
}

impl Deref for NetworkStackGuard<'_> {
    type Target = NetworkStack;

    fn deref(&self) -> &Self::Target {
        &self.state.stack
    }
}

impl DerefMut for NetworkStackGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state.stack
    }
}

impl NetworkStackGuard<'_> {
    /// @description 返回本轮 fixed-budget final cleanup 后是否仍有 backlog。
    /// @return 尚有 cleanup identity 时为 true；caller 必须回投 Network bit。
    /// @errors 无错误。
    pub(super) const fn cleanup_backlog(&self) -> bool {
        self.cleanup_backlog
    }
}

/// IPv4 protocol state 的唯一 owner。
///
/// 普通 task 竞争时通过 `TaskMutex` 睡眠；deferred poll 只尝试取得 owner，竞争或存在
/// payload loan 时立即回投。payload loan 只在短 owner transaction 中用同类型 placeholder
/// 取出/归还一个 socket；大块 copy 不持全局互斥 guard，只保留 O(1) loan-count 完整性证明。
pub(super) struct NetworkStackOwner {
    // OWNER: 唯一 protocol state；TaskMutex 只允许 task waiter 睡眠，deferred 只能 try。
    state: TaskMutex<NetworkStackState>,
    // OWNER: final Drop 的 exactly-once commands。spin 临界区只有 O(1) publish/pop，绝不
    // 持锁执行 protocol cleanup，否则 deferred 与 drop 会恢复无界 busy wait。
    pending_cleanup: Mutex<PendingCleanup<InetEndpoint, SOCKET_STORAGE_CAPACITY>>,
}

impl NetworkStackOwner {
    /// @description 创建唯一 IPv4 protocol owner 与空 final-cleanup ring。
    /// @param stack 已完整装配且尚未发布的唯一 NetworkStack。
    /// @return 无第二份 state 的 owner。
    /// @errors 不分配且无失败路径；零 cleanup capacity 由 const invariant fail-stop。
    pub(super) fn new(stack: NetworkStack) -> Self {
        Self {
            state: TaskMutex::new(NetworkStackState {
                stack,
                payload_loans: 0,
            }),
            pending_cleanup: Mutex::new(PendingCleanup::new()),
        }
    }

    /// @description 可睡眠取得唯一 NetworkStack owner。
    /// @return 完整协议状态 guard。
    /// @errors waiter metadata 分配失败时返回 `NoMemory`。
    pub(super) fn lock(&self) -> Result<NetworkStackGuard<'_>, SocketError> {
        self.state
            .lock()
            .map(|state| NetworkStackGuard {
                state,
                cleanup_backlog: false,
            })
            .map_err(|_| SocketError::NoMemory)
    }

    /// @description deferred poll 无等待地取得完整协议状态。
    /// @return owner 正忙或仍有 payload loan 时返回 `None`，由 caller 回投 Network bit。
    /// @errors 无错误且不分配 waiter；竞争只返回 `None`。
    pub(super) fn try_poll(&self) -> Option<NetworkStackGuard<'_>> {
        let mut state = self.state.try_lock()?;
        // cleanup 可以先处理其他 endpoint；同一 endpoint 的 final Drop 不可能与 active loan
        // 并存，因为 payload caller 的 live InetSocket 引用与 operation guard 覆盖完整 loan。
        let cleanup_backlog = self.drain_cleanup(&mut state.stack);
        if state.payload_loans != 0 {
            return None;
        }
        Some(NetworkStackGuard {
            state,
            cleanup_backlog,
        })
    }

    /// @description 在 stack owner 外执行一个 endpoint-local payload 操作。
    /// @param take 用同类型 placeholder 从 SocketSet 取出真实 socket。
    /// @param operation 只访问借出的 socket并执行 payload copy。
    /// @param restore 把真实 socket 归还原 handle，并消费 placeholder。
    /// @return operation 的结果。
    /// @errors owner waiter 预分配、take 或 operation 失败时返回 socket error；restore 不分配。
    pub(super) fn with_payload_loan<T, R>(
        &self,
        take: impl FnOnce(&mut NetworkStack) -> Result<T, SocketError>,
        operation: impl FnOnce(&mut T) -> Result<R, SocketError>,
        restore: impl FnOnce(&mut NetworkStack, T),
    ) -> Result<R, SocketError> {
        // restore 发生在 loan 已发布之后，必须在 take 前消除 waiter OOM 路径。
        let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| SocketError::NoMemory)?;
        let mut state = self.lock()?;
        let mut payload = take(&mut state)?;
        state.state.payload_loans = state
            .state
            .payload_loans
            .checked_add(1)
            .expect("network payload loan count overflow");
        drop(state);

        let result = operation(&mut payload);

        // poll 观察到 payload_loans 后只回投，不会持 owner 等待本 loan；prepared waiter
        // 仅覆盖与其他短 task transaction 的竞争。
        let mut state = NetworkStackGuard {
            state: self.state.lock_prepared(&mut wait),
            cleanup_backlog: false,
        };
        restore(&mut state, payload);
        state.state.payload_loans = state
            .state
            .payload_loans
            .checked_sub(1)
            .expect("network payload loan restored without membership");
        result
    }

    /// @description final InetSocket drop 无等待清理；owner 正忙时把固定 identity 交给下一轮 poll。
    /// @param endpoint exactly-once final Drop 保有且尚未释放 slot 的 identity。
    /// @return 无返回值；deferred publication 会同时回投 Network bit。
    /// @errors 超过 SocketSet capacity 表示 lifetime 不变量破坏并 fail-stop，不设 overflow fallback。
    pub(super) fn cleanup_or_defer(&self, endpoint: InetEndpoint) {
        if let Some(mut state) = self.state.try_lock() {
            super::cleanup_endpoint(&mut state.stack, endpoint);
            return;
        }
        let mut pending = self.pending_cleanup.lock();
        pending
            .publish(endpoint)
            .expect("final network cleanup identity exceeds unique socket capacity");
        drop(pending);
        crate::drivers::network::request_poll();
    }

    fn drain_cleanup(&self, stack: &mut NetworkStack) -> bool {
        for _ in 0..NETWORK_CLEANUP_BUDGET {
            let endpoint = self.pending_cleanup.lock().pop();
            let Some(endpoint) = endpoint else {
                return false;
            };
            super::cleanup_endpoint(stack, endpoint);
        }
        self.pending_cleanup.lock().has_pending()
    }
}

// OWNER: protocol_owner uniquely owns the one IPv4 stack and pending final-cleanup commands.
pub(super) static NETWORK_STACK: Once<NetworkStackOwner> = Once::new();
