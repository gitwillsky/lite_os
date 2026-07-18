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
    /// 放弃未发布 connection，并取回 capacity capability 供 listener owner 回滚。
    pub(super) fn into_reservation(self) -> BacklogReservation<T> {
        let Self { reservation, entry } = self;
        drop(entry);
        reservation
    }
}

/// AF_UNIX stream listener state lock 内的 pending queue 与 reservation ledger。
pub(super) struct StreamBacklog<T> {
    limit: usize,
    reserved: usize,
    next_sequence: u64,
    pending: FallibleMap<u64, T>,
}

impl<T> StreamBacklog<T> {
    /// 构造空 ledger；不按 backlog 深度预分配 storage。
    pub(super) const fn new(limit: usize) -> Self {
        Self {
            limit: if limit == 0 { 1 } else { limit },
            reserved: 0,
            next_sequence: 0,
            pending: FallibleMap::new(),
        }
    }

    /// 在 listener owner lock 下预留一个 connect slot，不执行分配。
    pub(super) fn reserve(&mut self) -> Result<BacklogReservation<T>, Full> {
        if self.reserved >= self.limit.saturating_sub(self.pending.len()) {
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
    pub(super) fn pop(&mut self) -> Option<T> {
        let sequence = *self.pending.first_key_value()?.0;
        self.pending.remove(&sequence)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
