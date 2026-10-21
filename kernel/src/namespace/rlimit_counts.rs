//! Linux rlimit ucounts 的 actual UserNS/kuid/资源与祖先收费；记录的 lifetime token 是唯一配额 owner。
use super::{NamespaceError, UserNamespace};
use crate::fallible_tree::FallibleMap;
use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

/// 已接入的实际 ucounts 资源；计数单位由各领域决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RlimitResource {
    LockedMemory,
    PendingSignals,
    /// io_uring ring 页占用的字节数（SQE 数组与 ring 区域，按页取整）。
    IoUringBytes,
}

/// 每个真实 UID 在每层 UserNS 上可持有的 io_uring ring 内存上限（字节）。
///
/// 这是本内核的策略值，不是 Linux ABI：Linux ≥5.12 用 memcg 记账而这里没有 memcg，缺少它时同一用户
/// 创建多个 ring（每个最大约 400 KiB）即可耗尽物理内存。超限的 `io_uring_setup` 返回 `ENOMEM`，与
/// memcg 记账失败一致。
pub(crate) const IO_URING_BYTES_PER_UID: u64 = 64 << 20;
/// NEWUSER 一次捕获创建者的 soft rlimits；immutable inherited maxima 不随其以后 setrlimit 改变。
#[derive(Clone, Copy)]
pub(crate) struct NamespaceRlimits {
    pub(crate) locked_memory: u64,
    pub(crate) pending_signals: u64,
}
impl NamespaceRlimits {
    fn maximum(self, resource: RlimitResource) -> u64 {
        match resource {
            RlimitResource::LockedMemory => self.locked_memory,
            RlimitResource::PendingSignals => self.pending_signals,
            RlimitResource::IoUringBytes => IO_URING_BYTES_PER_UID,
        }
        .min(i64::MAX as u64)
    }
}
pub(super) struct RlimitCounters {
    // OWNER: 本 UserNS 中各 (资源, real kuid) 的唯一 rlimit ledger；只保留非零条目。
    users: Mutex<FallibleMap<(RlimitResource, u32), u64>>,
    /// 下一层实际 ucounts 使用创建时捕获的最大值。
    parent_maxima: NamespaceRlimits,
}
impl RlimitCounters {
    pub(super) fn new(maxima: NamespaceRlimits) -> Self {
        Self {
            users: Mutex::new(FallibleMap::new()),
            parent_maxima: maxima,
        }
    }
    fn increment(
        &self,
        resource: RlimitResource,
        uid: u32,
        amount: u64,
        maximum: u64,
        exempt: bool,
    ) -> Result<(), NamespaceError> {
        let mut prepared: Option<crate::fallible_tree::NodeSlot<(RlimitResource, u32), u64>> = None;
        let key = (resource, uid);
        let result = loop {
            let mut users = self.users.lock();
            let next = users
                .get(&key)
                .copied()
                .unwrap_or(0)
                .checked_add(amount)
                .ok_or(NamespaceError::QuotaLimit)?;
            if next > i64::MAX as u64 || (!exempt && next > maximum) {
                break Err(NamespaceError::QuotaLimit);
            }
            if let Some(current) = users.get_mut(&key) {
                *current = next;
                break Ok(());
            }
            if let Some(node) = prepared.take() {
                users.commit_vacant(node.fill(key, next));
                break Ok(());
            }
            drop(users);
            prepared =
                Some(FallibleMap::try_reserve_node().map_err(|_| NamespaceError::OutOfMemory)?);
        };
        drop(prepared);
        result
    }
    fn decrement(&self, resource: RlimitResource, uid: u32, amount: u64) {
        let key = (resource, uid);
        let retired = {
            let mut users = self.users.lock();
            let current = users.get_mut(&key).expect("actual rlimit charge");
            *current = current
                .checked_sub(amount)
                .expect("rlimit charge underflow");
            if *current == 0 {
                users.take_entry(&key)
            } else {
                None
            }
        };
        drop(retired);
    }
}
/// 成功 publication 的实际配额 capability；消费/丢弃/销毁归还原收费者，非当前 caller。
#[derive(Debug)]
pub(crate) struct RlimitCharge {
    chain: Vec<(Arc<UserNamespace>, u32)>,
    charged: usize,
    resource: RlimitResource,
    amount: u64,
}
impl RlimitCharge {
    /// 当前 real UID 起点，随后按 immutable UserNS owner 链收费；全部 preparation 在短锁外。
    /// exempt 由实际领域 policy 给出；SHM 只认 initial CAP_IPC_LOCK，ordinary nonnegative
    /// source 越过软限但仍计费；RT/timer creation 不豁免。
    pub(crate) fn acquire(
        namespace: Arc<UserNamespace>,
        uid: u32,
        resource: RlimitResource,
        amount: u64,
        limit: u64,
        exempt: bool,
    ) -> Result<Self, NamespaceError> {
        let mut chain = Vec::new();
        chain
            .try_reserve_exact(usize::from(namespace.level()) + 1)
            .map_err(|_| NamespaceError::OutOfMemory)?;
        let (mut current, mut current_uid) = (namespace, uid);
        loop {
            let parent = current.parent();
            let parent_uid = current.owner();
            chain.push((current, current_uid));
            let Some(parent) = parent else {
                break;
            };
            current = parent;
            current_uid = parent_uid;
        }
        let mut charge = Self {
            chain,
            charged: 0,
            resource,
            amount,
        };
        // 当前调用单位由领域转换；祖先 max 按固定 Linux ucounts 原值传递。
        let mut maximum = limit.min(i64::MAX as u64);
        for (namespace, uid) in &charge.chain {
            namespace
                .rlimit_counts
                .increment(resource, *uid, amount, maximum, exempt)?;
            charge.charged += 1;
            maximum = namespace.rlimit_counts.parent_maxima.maximum(resource);
        }
        Ok(charge)
    }
}
impl Drop for RlimitCharge {
    fn drop(&mut self) {
        for (namespace, uid) in &self.chain[..self.charged] {
            namespace
                .rlimit_counts
                .decrement(self.resource, *uid, self.amount);
        }
    }
}
