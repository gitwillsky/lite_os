use core::marker::PhantomData;

use crate::fallible_tree::{FallibleMap, OutOfMemory, VacantEntry};

/// listener backlog 已满。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Full;

/// listener owner 已记录、尚未发布或回滚的唯一 capacity capability。
pub(super) struct BacklogReservation<T> {
    marker: PhantomData<fn() -> T>,
}

/// 已在 listener lock 外完成 queue-node 分配的 pending connection。
pub(super) struct StagedConnection<T> {
    reservation: BacklogReservation<T>,
    entry: VacantEntry<u64, T>,
}

impl<T> BacklogReservation<T> {
    /// 在 listener lock 外为 pending connection 准备 queue node。
    ///
    /// OOM 时把 reservation 还给 caller，确保 owner 能在 Drop 路径回滚 capacity。
    pub(super) fn try_stage(self, item: T) -> Result<StagedConnection<T>, (OutOfMemory, Self)> {
        let slot = match FallibleMap::try_reserve_node() {
            Ok(slot) => slot,
            Err(error) => return Err((error, self)),
        };
        Ok(StagedConnection {
            reservation: self,
            entry: slot.fill(0, item),
        })
    }
}

impl<T> StagedConnection<T> {
    /// 借用唯一尚未发布的实际 child；connect commit 用于安装 listener policy。
    pub(super) fn value(&self) -> &T {
        self.entry.value()
    }

    /// 放弃未发布 connection，并取回 capacity capability 供 listener owner 回滚。
    pub(super) fn into_reservation(self) -> BacklogReservation<T> {
        let Self { reservation, entry } = self;
        drop(entry);
        reservation
    }
}

/// AF_UNIX stream listener state lock 内的 pending queue 与 reservation ledger。
pub(super) struct ConnectionBacklog<T> {
    limit: usize,
    reserved: usize,
    next_sequence: u64,
    pending: FallibleMap<u64, T>,
}

impl<T> ConnectionBacklog<T> {
    /// 构造空 ledger；不按 backlog 深度预分配 storage。
    pub(super) const fn new(limit: usize) -> Self {
        Self {
            limit: limit.saturating_add(1),
            reserved: 0,
            next_sequence: 0,
            pending: FallibleMap::new(),
        }
    }

    /// relisten 修改唯一容量 policy；既有 pending/reservations 不丢弃，缩小后只阻止新 admission。
    pub(super) fn set_limit(&mut self, limit: usize) {
        self.limit = limit.saturating_add(1);
    }

    /// Linux unix_recvq_full 是 pending > backlog；实际容量为 backlog + 1，
    /// 未发布 reservation 同属该唯一 ledger，防止并发构造超额 admission。
    pub(super) fn capacity_available(&self) -> bool {
        self.reserved < self.limit.saturating_sub(self.pending.len())
    }

    /// 在 listener owner lock 下预留一个 connect slot，不执行分配。
    pub(super) fn reserve(&mut self) -> Result<BacklogReservation<T>, Full> {
        if !self.capacity_available() {
            return Err(Full);
        }
        self.reserved += 1;
        Ok(BacklogReservation {
            marker: PhantomData,
        })
    }

    /// 在 listener owner lock 下无分配发布 pending connection。
    pub(super) fn commit(&mut self, staged: StagedConnection<T>) {
        let StagedConnection {
            reservation: _,
            mut entry,
        } = staged;
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .expect("AF_UNIX backlog sequence exhausted");
        entry.set_key(sequence);
        self.pending.commit_vacant(entry);
        self.reserved = self
            .reserved
            .checked_sub(1)
            .expect("AF_UNIX backlog reservation underflow");
    }

    /// 在 listener owner lock 下释放失败 transaction 的 capacity。
    pub(super) fn rollback(&mut self, _reservation: BacklogReservation<T>) {
        self.reserved = self
            .reserved
            .checked_sub(1)
            .expect("AF_UNIX backlog reservation underflow");
    }

    /// 取出最早完成 publication 的 pending connection。
    pub(super) fn pop(&mut self) -> Option<VacantEntry<u64, T>> {
        let sequence = *self.pending.first_key_value()?.0;
        self.pending.take_entry(&sequence)
    }

    /// 回收全部真实 pending node；调用者必须在 listener guard 外析构返回值。
    /// reservation 仍由构造事务拥有；这里不修改其 ledger。
    pub(super) fn take_pending(&mut self) -> FallibleMap<u64, T> {
        core::mem::take(&mut self.pending)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
