//! inotify 事件队列：有界、相邻相同事件合并、溢出时只保留一个 `IN_Q_OVERFLOW`，以及线格式编码。
//!
//! 纯数据结构，不依赖 VFS 或调度器，可在 host 上单测。

use crate::fallible_tree::{FallibleMap, VacantEntry};
use alloc::boxed::Box;

pub(super) const IN_Q_OVERFLOW: u32 = 0x4000;
/// 队列最多容纳的事件数（Linux `max_queued_events` 缺省）；之后只追加一个溢出事件。
#[cfg(test)]
pub(super) const MAX_QUEUED_EVENTS: usize = 16384;
/// `struct inotify_event` 头部字节数：`wd`、`mask`、`cookie`、`len`。
const HEADER: usize = 16;

/// 一个待读取的事件。
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Event {
    pub(super) wd: i32,
    pub(super) mask: u32,
    pub(super) cookie: u32,
    pub(super) name: Box<[u8]>,
}

impl Event {
    /// Linux inotify event_compare：cookie 不参与相邻合并，IN_IGNORED 必须保留每次实际退休。
    fn same_notification(&self, other: &Self) -> bool {
        self.mask & 0x8000 == 0
            && self.mask == other.mask
            && self.wd == other.wd
            && self.name == other.name
    }

    /// 线格式总长度：头部加按 16 字节对齐、含结尾 NUL 的名字区（Linux `round_up(len + 1, 16)`）。
    pub(super) fn wire_len(&self) -> usize {
        HEADER
            + if self.name.is_empty() {
                0
            } else {
                (self.name.len() + 1).next_multiple_of(HEADER)
            }
    }

    /// 以固定栈 header/padding 交付一个记录；copy owner 决定用户 fault，不分配 staging buffer。
    pub(super) fn copy_with<E>(
        &self,
        mut write: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        let name_len = self.wire_len() - HEADER;
        let mut header = [0u8; HEADER];
        header[0..4].copy_from_slice(&self.wd.to_ne_bytes());
        header[4..8].copy_from_slice(&self.mask.to_ne_bytes());
        header[8..12].copy_from_slice(&self.cookie.to_ne_bytes());
        header[12..16].copy_from_slice(&(name_len as u32).to_ne_bytes());
        write(&header)?;
        if name_len != 0 {
            write(&self.name)?;
            write(&[0u8; HEADER][..name_len - self.name.len()])?;
        }
        Ok(())
    }
}

/// 一次 publish 的标准队列后果。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Pushed {
    Queued,
    Coalesced,
    Overflowed,
}

/// 当前用户缓冲区放不下首个完整记录。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct BufferTooSmall;

/// 锁外准备的普通 event node；没有 queue membership。
pub(super) type PreparedEvent = VacantEntry<u64, Event>;

/// 已移出队列的单个 read record；普通 node 在用户 copy 后锁外退休。
/// overflow 的实际 node 已归还队外 reserve，reader 只借其固定线格式事实。
pub(super) struct ReadRecord(Option<PreparedEvent>);
impl ReadRecord {
    pub(super) fn copy_with<E>(&self, write: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        match &self.0 {
            Some(node) => node.value().copy_with(write),
            None => Event {
                wd: -1,
                mask: IN_Q_OVERFLOW,
                cookie: 0,
                name: Box::default(),
            }
            .copy_with(write),
        }
    }
}

/// 唯一实际 event queue；有序 key 就是 FIFO sequence，不复制事件或生命周期。
pub(super) struct EventQueue {
    events: FallibleMap<u64, Event>,
    next_sequence: u64,
    // OWNER: max_queued_events 在 instance 创建时快照，后续 sysctl 不追溯更改已打开队列。
    maximum: usize,
    // OWNER: 预分配的唯一 overflow record node。Some 是队外 reserve，None 是真实队内记录；
    // 没有此 token 时节点 OOM 会连标准 IN_Q_OVERFLOW 都丢失。
    overflow: Option<PreparedEvent>,
    // OWNER: 同一 publish/retire transaction 的 wire byte 总量，FIONREAD 无需全队扫描。
    wire_bytes: usize,
}

impl EventQueue {
    /// admission 前准备唯一可复用 overflow node；OOM 尚未建立任何 instance。
    pub(super) fn try_new(maximum: usize) -> Result<Self, ()> {
        let overflow = Self::prepare(Event {
            wd: -1,
            mask: IN_Q_OVERFLOW,
            cookie: 0,
            name: Box::default(),
        })?;
        Ok(Self {
            events: FallibleMap::new(),
            next_sequence: 1,
            maximum,
            overflow: Some(overflow),
            wire_bytes: 0,
        })
    }
    /// 普通事件的全部 node allocation 在 queue guard 外执行。
    pub(super) fn prepare(event: Event) -> Result<PreparedEvent, ()> {
        FallibleMap::try_prepare(0, event).map_err(|_| ())
    }
    pub(super) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
    pub(super) fn pending_bytes(&self) -> usize {
        self.wire_bytes
    }

    /// 不分配发布 prepared event；None 表示锁外 allocation 失败，使用标准 overflow reserve。
    /// rejected node 交给 caller 在 guard 外析构；overflow 已排入时不会重复追加。
    pub(super) fn push(&mut self, event: Option<PreparedEvent>) -> (Pushed, Option<PreparedEvent>) {
        // Linux 先判 overflow，再比较 tail；满队列的相同 event 仍报告丢失。
        if self.events.len() < self.maximum
            && let Some(prepared) = &event
            && self
                .events
                .floor(&u64::MAX)
                .is_some_and(|(_, previous)| previous.same_notification(prepared.value()))
        {
            return (Pushed::Coalesced, event);
        }
        let (mut accepted, outcome, rejected) = match event {
            Some(prepared) if self.events.len() < self.maximum => (prepared, Pushed::Queued, None),
            rejected => {
                let Some(overflow) = self.overflow.take() else {
                    return (Pushed::Overflowed, rejected);
                };
                (overflow, Pushed::Overflowed, rejected)
            }
        };
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .expect("inotify event sequence exhausted");
        accepted.set_key(sequence);
        self.wire_bytes = self
            .wire_bytes
            .checked_add(accepted.value().wire_len())
            .expect("inotify wire byte count overflow");
        self.events.commit_vacant(accepted);
        (outcome, rejected)
    }

    /// 按 Linux get_one_event 原子取走首个完整记录；容量不足不消费，复制失败不放回。
    /// overflow node 在同一事务回到 reserve，用户 copy 期间的新 overflow 可以再次入队。
    pub(super) fn pop(&mut self, capacity: usize) -> Result<Option<ReadRecord>, BufferTooSmall> {
        let Some((&key, event)) = self.events.first_key_value() else {
            return Ok(None);
        };
        let length = event.wire_len();
        if length > capacity {
            return Err(BufferTooSmall);
        }
        let node = self
            .events
            .take_entry(&key)
            .expect("selected event disappeared");
        self.wire_bytes = self
            .wire_bytes
            .checked_sub(length)
            .expect("inotify wire byte count underflow");
        if node.value().mask == IN_Q_OVERFLOW {
            assert!(self.overflow.is_none(), "duplicate overflow queue record");
            self.overflow = Some(node);
            Ok(Some(ReadRecord(None)))
        } else {
            Ok(Some(ReadRecord(Some(node))))
        }
    }
}
