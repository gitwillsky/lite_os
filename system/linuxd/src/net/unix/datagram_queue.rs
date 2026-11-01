use crate::fallible_tree::{FallibleMap, VacantEntry};
use alloc::sync::Weak;

// Native max_dgram_qlen=10 在 qlen > limit 时阻塞，允许实际第十一条 admission。
pub(super) const MAX_DATAGRAMS: usize = 11;

/// 一条已在 transport guard 外完整预留的 datagram admission。
pub(super) struct PreparedDatagram<T>(VacantEntry<u64, T>);
impl<T> PreparedDatagram<T> {
    pub(super) fn new(item: T) -> Result<Self, T> {
        // reserve 不消费 payload，OOM 时 caller 可保持其 rights capability 并重试。
        match FallibleMap::try_reserve_node() {
            Ok(node) => Ok(Self(node.fill(0, item))),
            Err(_) => Err(item),
        }
    }
    pub(super) fn value_mut(&mut self) -> &mut T {
        self.0.value_mut()
    }
    pub(super) fn into_value(self) -> T {
        self.0.into_value()
    }
}
/// 由实际调用方提供 native peer policy 上限的 FIFO；AVL nodes 同时是 admission 与锁外 retirement token。
pub(super) struct DatagramQueue<T> {
    entries: FallibleMap<u64, T>,
    next_sequence: u64,
}
impl<T> DatagramQueue<T> {
    pub(super) const fn new() -> Self {
        Self {
            entries: FallibleMap::new(),
            next_sequence: 0,
        }
    }
    /// 只进行无分配 capacity/publication；full 保留完整 prepared capability。
    pub(super) fn push(
        &mut self,
        mut prepared: PreparedDatagram<T>,
        maximum: usize,
    ) -> Result<(), PreparedDatagram<T>> {
        if self.is_full(maximum) {
            return Err(prepared);
        }
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("AF_UNIX datagram sequence exhausted");
        prepared.0.set_key(self.next_sequence);
        self.entries.commit_vacant(prepared.0);
        Ok(())
    }
    /// 返回退休节点和 full→non-full 变化；caller 在 guard 外 into_value/析构。
    pub(super) fn pop(&mut self, maximum: usize) -> Option<(PreparedDatagram<T>, bool)> {
        let became_non_full = self.len() == maximum;
        let (&key, _) = self.entries.first_key_value()?;
        self.entries
            .take_entry(&key)
            .map(|node| (PreparedDatagram(node), became_non_full))
    }
    /// 借用当前实际首消息；caller 必须持有该队列的唯一 transport guard。
    pub(super) fn front(&self) -> Option<&T> {
        self.entries.first_key_value().map(|(_, item)| item)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(super) fn is_full(&self, maximum: usize) -> bool {
        self.len() >= maximum
    }
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn take_all(&mut self) -> FallibleMap<u64, T> {
        core::mem::take(&mut self.entries)
    }
}

pub(super) fn peer_identity_changed<T>(
    current: &Option<Weak<T>>,
    expected: &Option<Weak<T>>,
) -> bool {
    match (current, expected) {
        (Some(current), Some(expected)) => !Weak::ptr_eq(current, expected),
        (None, None) => false,
        _ => true,
    }
}
