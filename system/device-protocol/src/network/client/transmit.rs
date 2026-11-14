//! 同一 TX 锁拥有槽位、待发表描述符和完成回收。队列线程独占共享环发布。

use super::*;

enum Owner {
    Free,
    Reserved { ticket: u64 },
    Queued,
    Submitted { ticket: u64 },
}

struct Pending {
    record: Record,
    payload: Option<NetworkReceivePayload>,
}

pub(super) struct TransmitState {
    owners: Vec<Owner>,
    free: Vec<u16>,
    pending: VecDeque<Pending>,
    next_ticket: u64,
    // 拷贝期间批次仍处于 Queued。该标记防止其他调用者提前向设备发表尚未写完的槽。
    preparing: bool,
}

impl TransmitState {
    pub(super) fn new(slots: u16) -> Result<Self, Status> {
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(usize::from(slots))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        owners.resize_with(usize::from(slots), || Owner::Free);
        let mut free = Vec::new();
        free.try_reserve_exact(usize::from(slots))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        free.extend((0..slots).rev());
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(usize::from(slots))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Ok(Self {
            owners,
            free,
            pending,
            next_ticket: 1,
            preparing: false,
        })
    }

    pub(super) fn available(&self) -> bool {
        !self.free.is_empty()
    }

    pub(super) fn reserve(&mut self) -> Result<(u16, u64), NetworkError> {
        let &slot = self.free.last().ok_or(NetworkError::WouldBlock)?;
        let ticket = self.next_ticket;
        self.next_ticket = ticket.checked_add(1).ok_or(NetworkError::Device)?;
        self.free.pop();
        self.owners[usize::from(slot)] = Owner::Reserved { ticket };
        Ok((slot, ticket))
    }

    pub(super) fn enqueue(
        &mut self,
        slot: u16,
        ticket: u64,
        [length, checksum, segmentation]: [u64; 3],
        payload: &mut Option<NetworkReceivePayload>,
    ) -> Result<bool, NetworkError> {
        let owner = self
            .owners
            .get_mut(usize::from(slot))
            .ok_or(NetworkError::Device)?;
        if !matches!(*owner, Owner::Reserved { ticket: current } if current == ticket) {
            return Err(NetworkError::Device);
        }
        *owner = Owner::Queued;
        let first = self.pending.is_empty();
        // 每条记录对应独占的槽，FIFO 容量在装配时已覆盖全部槽。
        assert!(self.pending.len() < self.owners.len());
        self.pending.push_back(Pending {
            record: [u64::from(slot), ticket, length, checksum, segmentation],
            payload: payload.take(),
        });
        Ok(first)
    }

    pub(super) fn cancel(&mut self, slot: u16, ticket: u64) {
        let Some(owner) = self.owners.get_mut(usize::from(slot)) else {
            return;
        };
        if matches!(*owner, Owner::Reserved { ticket: current } if current == ticket) {
            *owner = Owner::Free;
            self.free.push(slot);
        }
    }

    fn publish(
        &mut self,
        count: usize,
        publish: impl FnOnce(&[Record]) -> Result<(), NetworkError>,
    ) -> Result<bool, NetworkError> {
        if count == 0 {
            return Ok(false);
        }
        let mut records = [[0; 5]; MAX_BATCH];
        for (record, pending) in records.iter_mut().zip(&self.pending).take(count) {
            *record = pending.record;
        }
        publish(&records[..count])?;
        // 队列仍持锁。设备即使立即完成，客户端也只会在状态变为 Submitted 后读 CQ。
        for [slot, ticket, ..] in &records[..count] {
            self.pending.pop_front();
            self.owners[*slot as usize] = Owner::Submitted { ticket: *ticket };
        }
        Ok(true)
    }

    fn complete(&mut self, slot: u16, ticket: u64) -> Result<(), NetworkError> {
        let owner = self
            .owners
            .get_mut(usize::from(slot))
            .ok_or(NetworkError::Device)?;
        if !matches!(*owner, Owner::Submitted { ticket: current } if current == ticket) {
            return Err(NetworkError::Device);
        }
        *owner = Owner::Free;
        self.free.push(slot);
        Ok(())
    }
}

impl RemoteQueue {
    pub(super) fn advance_transmit(
        &self,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        self.session.check()?;
        if self.session.activity.disconnected() {
            return Err(NetworkError::Disconnected);
        }
        let mut state = self.transmit.lock();
        self.session.check()?;
        if state.preparing {
            return Ok(NetworkCompletion {
                completed: 0,
                backlog: true,
                transmit_became_available: false,
            });
        }
        let mut done = self.reap_transmit(&mut state, budget)?;
        let count = state.pending.len().min(MAX_BATCH);
        if state
            .pending
            .iter()
            .take(count)
            .any(|pending| pending.payload.is_some())
        {
            let mut copies = [const { None }; MAX_BATCH];
            for (copy, pending) in copies.iter_mut().zip(&mut state.pending).take(count) {
                if let Some(payload) = pending.payload.take() {
                    *copy = Some((pending.record[0] as u16, pending.record[2] as usize, payload));
                }
            }
            state.preparing = true;
            drop(state);
            for (slot, length, payload) in copies.into_iter().flatten() {
                let start = self.transmit_memory.slot_address(slot) + length - payload.len();
                // SAFETY: enqueue 接管独占 CPU 写权限并核对长度。Preparing 排除并发发表，
                // CQ 只回收 Submitted 槽。源 RX 票据独立保活不可变载荷。
                let tail = unsafe { core::slice::from_raw_parts_mut(start as *mut u8, payload.len()) };
                tail.copy_from_slice(&payload);
                // payload 在 TX 锁外析构，归还另一队列的 RX 持有。
            }
            state = self.transmit.lock();
            state.preparing = false;
            self.session.check()?;
        }
        let published = state.publish(count, |records| {
            self.session.ring
                .push(Ring::Transmit, records)
                .map_err(|_| self.session.abort())
        })?;
        if published && state.pending.is_empty() {
            self.transmit_pending.store(false, Ordering::Release);
        }
        done.backlog |= !state.pending.is_empty();
        drop(state);
        if published || done.completed != 0 {
            self.session.kick()?;
        }
        Ok(done)
    }

    fn reap_transmit(
        &self,
        state: &mut TransmitState,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        let mut records = [[0; 5]; MAX_BATCH];
        let mut completed = 0;
        while completed < budget {
            let limit = MAX_BATCH.min(budget - completed);
            let count = self
                .session.ring
                .pop(Ring::Completion, &mut records[..limit])
                .map_err(|_| self.session.abort())?;
            if count == 0 {
                break;
            }
            for [slot, ticket, status, reserved, extra] in &records[..count] {
                if *status != 0 || *reserved != 0 || *extra != 0 {
                    return Err(self.session.abort());
                }
                let slot = u16::try_from(*slot).map_err(|_| self.session.abort())?;
                state.complete(slot, *ticket).map_err(|_| self.session.abort())?;
            }
            completed += count;
        }
        Ok(NetworkCompletion {
            completed,
            backlog: self
                .session.ring
                .pending(Ring::Completion)
                .map_err(|_| self.session.abort())?
                != 0,
            transmit_became_available: completed != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmit_preserves_submit_order_and_reclaims_only_published_tickets() {
        let mut state = TransmitState::new((MAX_BATCH + 1) as u16).unwrap();
        let reservations: Vec<_> = (0..=MAX_BATCH).map(|_| state.reserve().unwrap()).collect();
        for (index, &(slot, ticket)) in reservations.iter().rev().enumerate() {
            assert_eq!(
                state.enqueue(slot, ticket, [60, 0, 0], &mut None),
                Ok(index == 0)
            );
        }
        let &(first, ticket) = reservations.last().unwrap();
        assert_eq!(state.reserve(), Err(NetworkError::WouldBlock));
        state.cancel(first, ticket);
        assert_eq!(state.complete(first, ticket), Err(NetworkError::Device));
        assert_eq!(
            state.publish(MAX_BATCH, |_| Err(NetworkError::Device)),
            Err(NetworkError::Device)
        );
        assert_eq!(state.reserve(), Err(NetworkError::WouldBlock));

        assert_eq!(
            state.publish(MAX_BATCH, |records| {
                assert_eq!(records.len(), MAX_BATCH);
                for (record, &(slot, ticket)) in records.iter().zip(reservations.iter().rev()) {
                    assert_eq!(*record, [u64::from(slot), ticket, 60, 0, 0]);
                }
                Ok(())
            }),
            Ok(true)
        );
        assert_eq!(state.complete(first, ticket - 1), Err(NetworkError::Device));
        state.complete(first, ticket).unwrap();
        assert_eq!(state.reserve().unwrap().0, first);

        let (last, ticket) = reservations[0];
        assert_eq!(state.complete(last, ticket), Err(NetworkError::Device));
        assert_eq!(
            state.publish(1, |records| {
                assert_eq!(records, &[[u64::from(last), ticket, 60, 0, 0]]);
                Ok(())
            }),
            Ok(true)
        );
        state.complete(last, ticket).unwrap();
        assert_eq!(state.publish(0, |_| unreachable!()), Ok(false));
    }
}
