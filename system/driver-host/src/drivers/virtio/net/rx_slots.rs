use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Posted { head: u16 },
    DriverOwned,
    Retired,
}

struct ReceiveSlot<B> {
    buffer: B,
    state: SlotState,
}

pub(super) trait ReceiveQueue<B> {
    fn repost(&mut self, buffer: &B) -> Option<u16>;
    fn publish(&mut self, head: u16);
    fn retire_unpublished(&mut self, head: u16);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReceiveCompletion {
    pub(super) slot: u16,
    pub(super) length: usize,
}

/// 已从 head mapping exactly-once 取得、且 device length 已验证的 RX slot capability。
#[must_use = "claimed receive slot must be completed after VirtQueue recycles its descriptor"]
pub(super) struct ReceiveClaim<'a, B, const BUFFER_SIZE: usize> {
    // OWNER: 验证后的 claim 借用实际池；不能拿到另一个队列完成，也不能替换已验证长度。
    owner: &'a mut ReceiveSlots<B, BUFFER_SIZE>,
    slot_index: u16,
    length: usize,
}

impl<B, const BUFFER_SIZE: usize> ReceiveClaim<'_, B, BUFFER_SIZE> {
    /// 消费已验证的 claim，交出共享槽；不读 payload、不 repost。
    pub(super) fn complete(self) -> ReceiveCompletion {
        debug_assert_eq!(
            self.owner.slots[self.slot_index as usize].state,
            SlotState::DriverOwned
        );
        ReceiveCompletion {
            slot: self.slot_index,
            length: self.length,
        }
    }
}

pub(super) struct ReceiveSlots<B, const BUFFER_SIZE: usize> {
    slots: Vec<ReceiveSlot<B>>,
    by_head: Vec<Option<u16>>,
}

impl<B, const BUFFER_SIZE: usize> ReceiveSlots<B, BUFFER_SIZE> {
    pub(super) fn try_new(slot_capacity: usize, queue_size: usize) -> Option<Self> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(slot_capacity).ok()?;
        let mut by_head = Vec::new();
        by_head.try_reserve_exact(queue_size).ok()?;
        by_head.resize(queue_size, None);
        Some(Self { slots, by_head })
    }

    pub(super) fn insert_posted(&mut self, head: u16, buffer: B) -> Result<(), B> {
        let Some(mapping) = self.by_head.get_mut(head as usize) else {
            return Err(buffer);
        };
        if mapping.is_some() || self.slots.len() > u16::MAX as usize {
            return Err(buffer);
        }
        let slot_index = self.slots.len() as u16;
        self.slots.push(ReceiveSlot {
            buffer,
            state: SlotState::Posted { head },
        });
        *mapping = Some(slot_index);
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    /// 在 descriptor recycle 前验证并 claim adapter-owned RX slot。
    ///
    /// # Returns
    ///
    /// head 唯一对应 Posted slot 且 returned length 合法时返回 capability。
    ///
    /// # Errors
    ///
    /// unknown/duplicate head 或 device length 越界时返回 `None`，caller 必须 reset。
    pub(super) fn claim(
        &mut self,
        head: u16,
        used_length: usize,
        header_size: usize,
    ) -> Option<ReceiveClaim<'_, B, BUFFER_SIZE>> {
        let slot_index = self.claim_head(head)?;
        if !(header_size..=BUFFER_SIZE).contains(&used_length) {
            return None;
        }
        Some(ReceiveClaim {
            owner: self,
            slot_index,
            length: used_length - header_size,
        })
    }

    /// 冷路径授权共享池时借用槽的 buffer owner，不转移其 CPU/DMA ownership。
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "host lifecycle fixture does not open native VMO grants; the driver uses this cold accessor to authorize pool memory"
        )
    )]
    pub(super) fn buffer(&self, slot: usize) -> Option<&B> {
        self.slots.get(slot).map(|slot| &slot.buffer)
    }

    /// 客户端已结束字节借用后归还槽；重复/未知归还不会触碰 ring。
    pub(super) fn release<Q: ReceiveQueue<B>>(&mut self, queue: &mut Q, slot_index: u16) -> bool {
        if self
            .slots
            .get(slot_index as usize)
            .is_none_or(|slot| slot.state != SlotState::DriverOwned)
        {
            return false;
        }
        self.repost(queue, slot_index)
    }

    fn claim_head(&mut self, head: u16) -> Option<u16> {
        let slot_index = self.by_head.get_mut(head as usize)?.take()?;
        let slot = self.slots.get_mut(slot_index as usize)?;
        if slot.state != (SlotState::Posted { head }) {
            return None;
        }
        slot.state = SlotState::DriverOwned;
        Some(slot_index)
    }

    fn repost<Q: ReceiveQueue<B>>(&mut self, queue: &mut Q, slot_index: u16) -> bool {
        let slot = &mut self.slots[slot_index as usize];
        let Some(new_head) = queue.repost(&slot.buffer) else {
            slot.state = SlotState::Retired;
            return false;
        };
        let Some(mapping) = self.by_head.get_mut(new_head as usize) else {
            slot.state = SlotState::Retired;
            queue.retire_unpublished(new_head);
            return false;
        };
        if mapping.is_some() {
            slot.state = SlotState::Retired;
            queue.retire_unpublished(new_head);
            return false;
        }
        slot.state = SlotState::Posted { head: new_head };
        *mapping = Some(slot_index);
        queue.publish(new_head);
        true
    }

    #[cfg(test)]
    fn ownership_counts(&self) -> (usize, usize, usize) {
        self.slots.iter().fold((0, 0, 0), |mut counts, slot| {
            match slot.state {
                SlotState::Posted { .. } => counts.0 += 1,
                SlotState::DriverOwned => counts.1 += 1,
                SlotState::Retired => counts.2 += 1,
            }
            counts
        })
    }

    #[cfg(test)]
    fn assert_invariant(&self) {
        let mut mapped = alloc::vec![false; self.slots.len()];
        for (head, slot_index) in self.by_head.iter().enumerate() {
            let Some(slot_index) = slot_index else {
                continue;
            };
            let slot_index = *slot_index as usize;
            assert!(slot_index < self.slots.len());
            assert!(!core::mem::replace(&mut mapped[slot_index], true));
            assert_eq!(
                self.slots[slot_index].state,
                SlotState::Posted { head: head as u16 }
            );
        }
        for (slot_index, slot) in self.slots.iter().enumerate() {
            assert_eq!(
                mapped[slot_index],
                matches!(slot.state, SlotState::Posted { .. })
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReceiveQueue, ReceiveSlots};
    use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
    type TestSlots = ReceiveSlots<Box<[u8; 32]>, 32>;
    fn buffer() -> Box<[u8; 32]> {
        Box::new([0; 32])
    }
    struct MockQueue {
        repost_heads: VecDeque<Option<u16>>,
        published: Vec<u16>,
        retired: Vec<u16>,
    }
    impl MockQueue {
        fn returning(heads: impl IntoIterator<Item = Option<u16>>) -> Self {
            Self {
                repost_heads: heads.into_iter().collect(),
                published: Vec::new(),
                retired: Vec::new(),
            }
        }
    }
    impl ReceiveQueue<Box<[u8; 32]>> for MockQueue {
        fn repost(&mut self, _: &Box<[u8; 32]>) -> Option<u16> {
            self.repost_heads.pop_front().flatten()
        }
        fn publish(&mut self, head: u16) {
            self.published.push(head);
        }
        fn retire_unpublished(&mut self, head: u16) {
            self.retired.push(head);
        }
    }
    #[test]
    fn malformed_completion_is_claimed_for_terminal_reset_without_repost() {
        let mut slots = TestSlots::try_new(1, 2).unwrap();
        slots.insert_posted(0, buffer()).unwrap();
        assert!(slots.claim(0, 4, 12).is_none());
        assert_eq!(slots.len(), 1);
        assert_eq!(slots.ownership_counts(), (0, 1, 0));
        slots.assert_invariant();
    }
    #[test]
    fn normal_completion_holds_payload_until_explicit_release() {
        let mut slots = TestSlots::try_new(1, 2).unwrap();
        slots.insert_posted(0, buffer()).unwrap();
        let mut queue = MockQueue::returning([Some(1)]);
        let completion = slots.claim(0, 16, 12).unwrap().complete();
        assert_eq!((completion.slot, completion.length), (0, 4));
        assert_eq!(slots.ownership_counts(), (0, 1, 0));
        assert!(queue.published.is_empty());
        assert!(slots.release(&mut queue, completion.slot));
        assert_eq!(slots.ownership_counts(), (1, 0, 0));
        assert_eq!(queue.published, [1]);
        assert!(!slots.release(&mut queue, completion.slot));
        assert!(!slots.release(&mut queue, 9));
        slots.assert_invariant();
    }
    #[test]
    fn malformed_batch_never_reaches_descriptor_repost() {
        const CAPACITY: usize = 32;
        let mut slots = TestSlots::try_new(CAPACITY, CAPACITY * 2).unwrap();
        for head in 0..CAPACITY as u16 {
            slots.insert_posted(head, buffer()).unwrap();
        }
        for head in 0..CAPACITY as u16 {
            let length = if (head as usize).is_multiple_of(2) {
                4
            } else {
                33
            };
            assert!(slots.claim(head, length, 12).is_none());
            slots.assert_invariant();
        }
        assert_eq!(slots.ownership_counts(), (0, CAPACITY, 0));
    }
    #[test]
    fn repost_capacity_failure_explicitly_retires_the_claimed_slot() {
        let mut slots = TestSlots::try_new(1, 2).unwrap();
        slots.insert_posted(0, buffer()).unwrap();
        let mut queue = MockQueue::returning([None]);
        let completion = slots.claim(0, 12, 12).unwrap().complete();
        assert!(!slots.release(&mut queue, completion.slot));
        assert_eq!(slots.ownership_counts(), (0, 0, 1));
        assert!(queue.published.is_empty());
        slots.assert_invariant();
    }
    #[test]
    fn duplicate_completion_does_not_claim_the_reposted_slot() {
        let mut slots = TestSlots::try_new(1, 2).unwrap();
        slots.insert_posted(0, buffer()).unwrap();
        let mut queue = MockQueue::returning([Some(1)]);
        let completion = slots.claim(0, 12, 12).unwrap().complete();
        assert!(slots.release(&mut queue, completion.slot));
        assert!(slots.claim(0, 12, 12).is_none());
        assert_eq!(slots.ownership_counts(), (1, 0, 0));
        assert_eq!(queue.published, [1]);
        slots.assert_invariant();
    }
    #[test]
    fn conflicting_repost_head_retires_only_the_completed_slot() {
        let mut slots = TestSlots::try_new(2, 2).unwrap();
        slots.insert_posted(0, buffer()).unwrap();
        slots.insert_posted(1, buffer()).unwrap();
        let mut queue = MockQueue::returning([Some(1)]);
        let completion = slots.claim(0, 12, 12).unwrap().complete();
        assert!(!slots.release(&mut queue, completion.slot));
        assert_eq!(slots.ownership_counts(), (1, 0, 1));
        assert!(queue.published.is_empty());
        assert_eq!(queue.retired, [1]);
        slots.assert_invariant();
    }
}
