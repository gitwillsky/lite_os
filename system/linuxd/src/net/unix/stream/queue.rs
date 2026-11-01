//! 实际 stream chunk FIFO；节点在 guard 外准备，追加/摘首 O(1)，退休仍转交同一 allocation。
use super::MessageFrame;
use alloc::boxed::Box;
use core::ptr::NonNull;

#[derive(Default)]
pub(super) struct FrameQueue {
    head: Option<Box<MessageFrame>>,
    // SAFETY OWNER: tail 只指向 head 链拥有的最后一个稳定 Box；不另持所有权。
    tail: Option<NonNull<MessageFrame>>,
}
// SAFETY: 移动 queue 不移动 Box pointee；MessageFrame 的 payload/SCM/费用均可跨线程移交。
unsafe impl Send for FrameQueue where MessageFrame: Send {}
// SAFETY: shared borrow 只走 Box 链，不访问 tail；任何 link mutation 都要求独占 borrow。
unsafe impl Sync for FrameQueue where MessageFrame: Sync {}
impl FrameQueue {
    pub(super) fn is_empty(&self) -> bool {
        self.head.is_none()
    }
    pub(super) fn front(&self) -> Option<&MessageFrame> {
        self.head.as_deref()
    }
    pub(super) fn front_mut(&mut self) -> Option<&mut MessageFrame> {
        self.head.as_deref_mut()
    }
    /// 只接收完整且无链的真实节点；不会分配、复制 payload 或释放旧节点。
    #[inline]
    pub(super) fn push(&mut self, mut frame: Box<MessageFrame>) {
        assert!(
            frame.next.is_none(),
            "stream publication carries an owned chain"
        );
        let tail = NonNull::from(frame.as_mut());
        if let Some(mut previous) = self.tail {
            // SAFETY: 独占 queue 排除 iterator；previous 是链内最后一个存活 Box，next 必为 None。
            unsafe { previous.as_mut().next = Some(frame) };
        } else {
            self.head = Some(frame);
        }
        self.tail = Some(tail);
    }
    /// 无分配摘首，断开拥有的 next；空队列同时清除非 owning tail。
    #[inline]
    pub(super) fn pop(&mut self) -> Option<Box<MessageFrame>> {
        let mut frame = self.head.take()?;
        self.head = frame.next.take();
        if self.head.is_none() {
            self.tail = None;
        }
        Some(frame)
    }
    pub(super) fn iter(&self) -> FrameIter<'_> {
        FrameIter(self.head.as_deref())
    }
    /// controls 仅访问本次 read plan 已扫描到的实际位置，不维护第二份索引。
    pub(super) fn at_mut(&mut self, index: usize) -> Option<&mut MessageFrame> {
        let mut frame = self.head.as_deref_mut()?;
        for _ in 0..index {
            frame = frame.next.as_deref_mut()?;
        }
        Some(frame)
    }
}
impl Drop for FrameQueue {
    fn drop(&mut self) {
        // 大队列必须迭代退休，不能递归析构 Box next 耗尽 kernel stack。
        while let Some(frame) = self.pop() {
            drop(frame);
        }
    }
}
pub(super) struct FrameIter<'a>(Option<&'a MessageFrame>);
impl<'a> Iterator for FrameIter<'a> {
    type Item = &'a MessageFrame;
    fn next(&mut self) -> Option<Self::Item> {
        let frame = self.0?;
        self.0 = frame.next.as_deref();
        Some(frame)
    }
}
