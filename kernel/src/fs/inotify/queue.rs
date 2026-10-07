//! inotify 事件队列：有界、相邻相同事件合并、溢出时只保留一个 `IN_Q_OVERFLOW`，以及线格式编码。
//!
//! 纯数据结构，不依赖 VFS 或调度器，可在 host 上单测。

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};

pub(super) const IN_Q_OVERFLOW: u32 = 0x4000;
/// 队列最多容纳的事件数（Linux `max_queued_events` 缺省）；之后只追加一个溢出事件。
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
    /// 线格式总长度：头部加按 16 字节对齐、含结尾 NUL 的名字区（Linux `round_up(len + 1, 16)`）。
    pub(super) fn wire_len(&self) -> usize {
        HEADER
            + if self.name.is_empty() {
                0
            } else {
                (self.name.len() + 1).next_multiple_of(HEADER)
            }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let name_len = self.wire_len() - HEADER;
        out.extend_from_slice(&self.wd.to_ne_bytes());
        out.extend_from_slice(&self.mask.to_ne_bytes());
        out.extend_from_slice(&self.cookie.to_ne_bytes());
        out.extend_from_slice(&(name_len as u32).to_ne_bytes());
        out.extend_from_slice(&self.name);
        out.resize(out.len() + name_len - self.name.len(), 0);
    }
}

/// 队列容量或内存不足，事件被丢弃并改记为溢出。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Pushed {
    Queued,
    /// 与队尾相同，已合并。
    Coalesced,
    /// 队列已满或分配失败：事件丢弃，队尾是（或刚追加了）溢出事件。
    Overflowed,
}

#[derive(Default)]
pub(super) struct EventQueue {
    events: VecDeque<Event>,
}

impl EventQueue {
    pub(super) const fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 全部排队事件的线格式总字节数（`FIONREAD`）。
    pub(super) fn pending_bytes(&self) -> usize {
        self.events.iter().map(Event::wire_len).sum()
    }

    /// 追加一个事件。
    ///
    /// 与队尾完全相同的事件被合并（Linux `inotify_merge`）；队列满或分配失败时丢弃该事件，并保证队尾
    /// 是一个 `IN_Q_OVERFLOW`（wd 为 -1），已经是则不再追加。
    pub(super) fn push(&mut self, event: Event) -> Pushed {
        if self.events.back() == Some(&event) {
            return Pushed::Coalesced;
        }
        if self.events.len() < MAX_QUEUED_EVENTS && self.events.try_reserve(1).is_ok() {
            self.events.push_back(event);
            return Pushed::Queued;
        }
        if self
            .events
            .back()
            .is_none_or(|last| last.mask != IN_Q_OVERFLOW)
        {
            // 溢出事件允许超出上限一个：它是用户得知“丢了事件”的唯一途径。
            if self.events.try_reserve(1).is_ok() {
                self.events.push_back(Event {
                    wd: -1,
                    mask: IN_Q_OVERFLOW,
                    cookie: 0,
                    name: Box::default(),
                });
            }
        }
        Pushed::Overflowed
    }

    /// 编码队首起尽可能多的完整事件，总长度不超过 `capacity`；不出队。
    ///
    /// 出队与编码分开：调用者先把字节交给用户，成功后才 [`Self::discard`]，用户缓冲 fault 不会丢事件。
    ///
    /// # Returns
    ///
    /// 编码字节与事件数；队列非空但第一个事件就放不下返回 `Err(())`（`EINVAL`）；队列为空返回空结果。
    pub(super) fn encode_prefix(&self, capacity: usize) -> Result<(Vec<u8>, usize), ()> {
        let mut bytes = Vec::new();
        let mut count = 0;
        let mut total = 0;
        for event in &self.events {
            let length = event.wire_len();
            if total + length > capacity {
                break;
            }
            bytes.try_reserve(length).map_err(|_| ())?;
            event.encode(&mut bytes);
            total += length;
            count += 1;
        }
        if count == 0 && !self.events.is_empty() {
            return Err(());
        }
        Ok((bytes, count))
    }

    /// 丢弃队首 `count` 个已交付的事件。
    pub(super) fn discard(&mut self, count: usize) {
        self.events.drain(..count);
    }
}
