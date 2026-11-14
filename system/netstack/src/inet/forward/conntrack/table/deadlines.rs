//! 连接的到期节点属于原始键所在分区。域只索引各分区的最早到期项。
use super::super::{Deadline, Key};
use super::Tables;
use fallible_tree::{FallibleMap, VacantEntry};
use ::tcp::time::Instant;

pub(super) const NO_SHARD: usize = usize::MAX;

pub(super) struct DeadlineQueue {
    nodes: FallibleMap<Deadline, ()>,
    // 只缓存本队列的首键。删除或换键移走首节点时才重查树；没有每连接的第二份计时记录。
    first: Option<Deadline>,
}

impl DeadlineQueue {
    pub(super) const fn new() -> Self {
        Self {
            nodes: FallibleMap::new(),
            first: None,
        }
    }

    fn insert(&mut self, node: VacantEntry<Deadline, ()>) {
        let deadline = *node.key();
        self.nodes.commit_vacant(node);
        if self.first.is_none_or(|first| deadline < first) {
            self.first = Some(deadline);
        }
    }

    fn remove(&mut self, deadline: Deadline) -> VacantEntry<Deadline, ()> {
        let node = self.nodes.take_entry(&deadline).expect("scheduled flow");
        if self.first == Some(deadline) {
            self.first = self.nodes.first_key_value().map(|(&first, ())| first);
        }
        node
    }

    fn replace(&mut self, from: Deadline, to: Deadline) {
        let mut node = self.nodes.take_entry(&from).expect("scheduled flow");
        node.set_key(to);
        self.nodes.commit_vacant(node);
        if self.first.is_some_and(|first| to <= first) {
            self.first = Some(to);
        } else if self.first == Some(from) {
            self.first = self.nodes.first_key_value().map(|(&first, ())| first);
        }
    }
}

impl Tables {
    /// 提交已经预留的到期节点，不能在 TX 之后再分配。
    pub(in crate::inet::forward::conntrack) fn schedule(
        &mut self,
        slot: usize,
        node: VacantEntry<Deadline, ()>,
    ) {
        let queue = &mut self.shards[slot].deadlines;
        let previous = queue.first;
        queue.insert(node);
        self.refresh_winners(slot, previous);
    }

    /// 在同一分区复用到期节点。新期限和连接 ID 随连接更新。
    pub(in crate::inet::forward::conntrack) fn reschedule(
        &mut self,
        slot: usize,
        from: Deadline,
        to: Deadline,
    ) {
        let queue = &mut self.shards[slot].deadlines;
        let previous = queue.first;
        queue.replace(from, to);
        self.refresh_winners(slot, previous);
    }

    pub(in crate::inet::forward::conntrack) fn unschedule(&mut self, key: Key, deadline: Deadline) {
        let (slot, _) = self.location(key);
        let queue = &mut self.shards[slot].deadlines;
        let previous = queue.first;
        drop(queue.remove(deadline));
        self.refresh_winners(slot, previous);
    }

    fn refresh_winners(&mut self, slot: usize, previous: Option<Deadline>) {
        let head = self.shards[slot].deadlines.first;
        if head == previous {
            return;
        }
        let mut node = self.deadline_winners.len() / 2 + slot;
        self.deadline_winners[node] = if head.is_some() { slot } else { NO_SHARD };
        while node > 1 {
            node /= 2;
            let left = self.deadline_winners[2 * node];
            let right = self.deadline_winners[2 * node + 1];
            let winner = match (left, right) {
                (NO_SHARD, _) => right,
                (_, NO_SHARD) => left,
                _ if self.shards[left].deadlines.first <= self.shards[right].deadlines.first => {
                    left
                }
                _ => right,
            };
            if winner != slot && winner == self.deadline_winners[node] {
                // 只有 slot 的首键有变化。子树仍由同一个其他分区领先，祖先无需重算。
                break;
            }
            self.deadline_winners[node] = winner;
        }
    }

    /// 全域最早的实际截止。空闲调度不遍历分区或连接。
    pub(in crate::inet::forward::conntrack) fn next_deadline(&self) -> Option<Instant> {
        let &slot = self.deadline_winners.get(1)?;
        self.shards.get(slot)?.deadlines.first.map(|(at, _)| at)
    }

    /// 从上次服务的后继开始选择到期分区。最早的分区决定唤醒时间，轮转决定积压处理次序。
    pub(in crate::inet::forward::conntrack) fn next_ready_shard(
        &mut self,
        now: Instant,
    ) -> Option<usize> {
        if self.next_deadline()? > now {
            return None;
        }
        let (before, after) = self.shards.split_at(self.retire_from);
        let ready = |shard: &super::Shard| shard.deadlines.first.is_some_and(|(at, _)| at <= now);
        let slot = after
            .iter()
            .position(ready)
            .map(|offset| self.retire_from + offset)
            .or_else(|| before.iter().position(ready))
            .expect("deadline winner must be ready");
        self.retire_from = (slot + 1) % self.shards.len();
        Some(slot)
    }

    /// 读取当前分区的到期项。最后一条连接清除时可同时退休整个索引。
    pub(in crate::inet::forward::conntrack) fn due_in_shard(
        &self,
        slot: usize,
        now: Instant,
    ) -> Option<Deadline> {
        self.shards
            .get(slot)?
            .deadlines
            .first
            .filter(|(at, _)| *at <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inet::forward::conntrack::{
        Datagram, FlowHash, Flows, Packet, Protocol, RETIRE_BATCH,
    };
    use core::net::Ipv4Addr;

    #[test]
    fn connection_retirement_rotates_between_busy_shards_and_preserves_the_next_deadline() {
        fn confirm(flows: &mut Flows, datagram: Datagram, millis: i64) {
            let now = Instant::from_millis(millis);
            let (_, tracked) = flows
                .observe(Packet::Data(datagram), None, now)
                .unwrap()
                .unwrap();
            let tracked = flows.reserve(tracked, now).unwrap();
            flows.confirm(tracked, now);
        }

        let mut flows = Flows::new();
        flows.tables = Tables::new(FlowHash([0x17; 16]));
        flows.tables.prepare().unwrap();
        fn in_shard(tables: &Tables, slot: usize) -> impl Iterator<Item = Datagram> + '_ {
            (1024..u16::MAX)
                .map(|port| {
                    Datagram::ports(
                        Protocol::Udp,
                        (Ipv4Addr::new(192, 0, 2, 1).into(), port),
                        (Ipv4Addr::new(198, 51, 100, 2).into(), 53),
                    )
                    .unwrap()
                })
                .filter(move |datagram| tables.location(datagram.key).0 == slot)
        }
        let first: alloc::vec::Vec<_> = in_shard(&flows.tables, 0)
            .take(2 * RETIRE_BATCH + 1)
            .collect();
        let (expired, refreshed) = {
            let mut other = in_shard(&flows.tables, 1);
            (other.next().unwrap(), other.next().unwrap())
        };
        for datagram in first {
            confirm(&mut flows, datagram, 0);
        }
        confirm(&mut flows, expired, 1);
        confirm(&mut flows, refreshed, 1);
        confirm(
            &mut flows,
            Datagram {
                reversed: !refreshed.reversed,
                ..refreshed
            },
            1000,
        );

        let now = Instant::from_millis(32_000);
        assert_eq!(flows.next_deadline(), Some(Instant::from_millis(30_000)));
        assert!(flows.retire(now, u64::MAX));
        assert!(flows.tables.flow(expired.key).is_some());
        assert!(flows.retire(now, 0));
        assert!(flows.tables.flow(expired.key).is_none());
        assert!(!flows.retire(now, u64::MAX));
        assert_eq!(flows.next_deadline(), Some(Instant::from_millis(121_000)));
        assert!(!flows.retire(Instant::from_millis(121_000), u64::MAX));
        assert!(flows.next_deadline().is_none());
    }
}
