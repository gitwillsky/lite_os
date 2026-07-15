use alloc::{sync::Arc, vec::Vec};
use core::num::NonZeroUsize;
use spin::Mutex;

mod eventfd;
pub(crate) use eventfd::{EventFd, EventFdRead, EventFdWrite};

pub(crate) const PIPE_BUF: usize = 4096;
const PIPE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
const NOTIFICATION_CAPACITY: NonZeroUsize = NonZeroUsize::MIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum PipeDirection {
    Read,
    Write,
}

/// @description blocking pipe I/O 的精确完成条件；写等待携带本次原子写所需的完整容量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeWaitCondition {
    Readable,
    Writable { minimum: usize },
}

impl PipeWaitCondition {
    /// @description 返回该 blocking condition 所属的 endpoint direction，并验证写容量范围。
    ///
    /// @return read/write endpoint direction；非法写容量破坏 kernel 调用契约并 fail-stop。
    pub(crate) fn direction(self) -> PipeDirection {
        match self {
            Self::Readable => PipeDirection::Read,
            Self::Writable { minimum } => {
                assert!((1..=PIPE_BUF).contains(&minimum));
                PipeDirection::Write
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeRead {
    Bytes(usize),
    Empty,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeWrite {
    Bytes(usize),
    Full,
    Broken,
}

/// @description byte ring 写入语义；匿名 pipe 保证 `PIPE_BUF` 原子性，stream socket 允许短写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeWriteMode {
    Pipe,
    Stream,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PipePollState {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    pub(crate) hangup: bool,
    pub(crate) error: bool,
    pub(crate) write_capacity: usize,
}

impl PipePollState {
    /// @description 判断同一 PipeState snapshot 是否满足 blocking I/O 的精确完成条件。
    ///
    /// @param condition read data/EOF 或一笔 `PIPE_BUF` 范围内原子写所需的完整容量。
    /// @return 条件已满足或 write endpoint 已 broken 时返回 true。
    pub(crate) fn satisfies(self, condition: PipeWaitCondition) -> bool {
        match condition {
            PipeWaitCondition::Readable => self.readable,
            PipeWaitCondition::Writable { minimum } => self.error || self.write_capacity >= minimum,
        }
    }
}

pub(crate) trait PipeNotifier: Send + Sync {
    fn notify(&self, pipe: &Arc<Pipe>);
}

struct PipeState {
    bytes: Vec<u8>,
    head: usize,
    length: usize,
    readers: usize,
    writers: usize,
    read_generation: u64,
    write_generation: u64,
}

/// @description data/notification Pipe 的唯一 byte ring、generation 与 endpoint lifecycle owner。
pub(crate) struct Pipe {
    // Pipe owner 分配一次并由两个 endpoint 共享；缺失时 read/write fd 会报告不同 pipe inode。
    object_id: u64,
    state: Mutex<PipeState>,
    notifier: Arc<dyn PipeNotifier>,
}

impl Pipe {
    /// @description 创建一对唯一 read/write endpoint。
    ///
    /// @param notifier 在状态变为可读、可写、EOF 或 broken 时唤醒 task registry。
    /// @return 两个 endpoint；kernel heap 不足返回错误。
    pub(crate) fn pair(
        notifier: Arc<dyn PipeNotifier>,
    ) -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
        Self::pair_with_capacity(notifier, PIPE_CAPACITY)
    }

    /// @description 创建只承载合并 readiness token 的一字节 Pipe endpoints。
    ///
    /// @param notifier 与 data Pipe 共用的 task wait-registry 通知 seam。
    /// @return 两个 endpoint；kernel heap 不足返回错误。
    pub(crate) fn notification_pair(
        notifier: Arc<dyn PipeNotifier>,
    ) -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
        Self::pair_with_capacity(notifier, NOTIFICATION_CAPACITY)
    }

    fn pair_with_capacity(
        notifier: Arc<dyn PipeNotifier>,
        capacity: NonZeroUsize,
    ) -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
        let capacity = capacity.get();
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| ())?;
        bytes.resize(capacity, 0);
        let pipe = Arc::try_new(Self {
            object_id: crate::id::next_runtime_object_id(),
            state: Mutex::new(PipeState {
                bytes,
                head: 0,
                length: 0,
                readers: 1,
                writers: 1,
                read_generation: crate::sync::next_readiness_generation(),
                write_generation: crate::sync::next_readiness_generation(),
            }),
            notifier,
        })
        .map_err(|_| ())?;
        let read = Arc::try_new(PipeEnd {
            pipe: pipe.clone(),
            direction: PipeDirection::Read,
        })
        .map_err(|_| ())?;
        let write = Arc::try_new(PipeEnd {
            pipe,
            direction: PipeDirection::Write,
        })
        .map_err(|_| ())?;
        Ok((read, write))
    }

    pub(crate) fn identity(pipe: &Arc<Self>) -> usize {
        Arc::as_ptr(pipe) as usize
    }

    pub(crate) fn object_id(&self) -> u64 {
        self.object_id
    }

    /// @description 在 Pipe owner lock 下复查 blocking I/O 的精确完成条件。
    ///
    /// @param condition read data/EOF 或一笔原子写所需的完整容量。
    /// @return 当前状态满足条件时返回 true。
    pub(crate) fn wait_ready(&self, condition: PipeWaitCondition) -> bool {
        self.poll_state(condition.direction()).satisfies(condition)
    }

    pub(crate) fn poll_state(&self, direction: PipeDirection) -> PipePollState {
        let state = self.state.lock();
        match direction {
            PipeDirection::Read => PipePollState {
                readable: state.length != 0 || state.writers == 0,
                writable: false,
                hangup: state.writers == 0,
                error: false,
                write_capacity: 0,
            },
            PipeDirection::Write => PipePollState {
                readable: false,
                writable: state.readers != 0 && state.length != state.bytes.len(),
                hangup: false,
                error: state.readers == 0,
                write_capacity: state.bytes.len() - state.length,
            },
        }
    }

    /// @description 返回指定 endpoint 最近一次可观察状态变化的全局 generation。
    ///
    /// @param direction read 侧跟踪 data/EOF，write 侧跟踪 space/broken-pipe。
    /// @return 跨 I/O source 可比较的 generation。
    pub(crate) fn readiness_generation(&self, direction: PipeDirection) -> u64 {
        let state = self.state.lock();
        match direction {
            PipeDirection::Read => state.read_generation,
            PipeDirection::Write => state.write_generation,
        }
    }

    fn read(self: &Arc<Self>, output: &mut [u8]) -> PipeRead {
        let result = {
            let mut state = self.state.lock();
            if state.length == 0 {
                if state.writers == 0 {
                    PipeRead::Eof
                } else {
                    PipeRead::Empty
                }
            } else {
                let count = output.len().min(state.length);
                if count != 0 {
                    let capacity = state.bytes.len();
                    let head = state.head;
                    let first = count.min(capacity - head);
                    output[..first].copy_from_slice(&state.bytes[head..head + first]);
                    let second = count - first;
                    if second != 0 {
                        output[first..count].copy_from_slice(&state.bytes[..second]);
                    }
                    let next = head + count;
                    state.head = if next >= capacity {
                        next - capacity
                    } else {
                        next
                    };
                    state.length -= count;
                }
                state.write_generation = crate::sync::next_readiness_generation();
                PipeRead::Bytes(count)
            }
        };
        if matches!(result, PipeRead::Bytes(_)) {
            self.notifier.notify(self);
        }
        result
    }

    fn write(self: &Arc<Self>, input: &[u8], mode: PipeWriteMode) -> PipeWrite {
        let result = {
            let mut state = self.state.lock();
            if state.readers == 0 {
                PipeWrite::Broken
            } else {
                let available = state.bytes.len() - state.length;
                if available == 0
                    || mode == PipeWriteMode::Pipe
                        && input.len() <= PIPE_BUF
                        && available < input.len()
                {
                    PipeWrite::Full
                } else {
                    let count = available.min(input.len());
                    if count != 0 {
                        let capacity = state.bytes.len();
                        let tail = state.head + state.length;
                        let tail = if tail >= capacity {
                            tail - capacity
                        } else {
                            tail
                        };
                        let first = count.min(capacity - tail);
                        state.bytes[tail..tail + first].copy_from_slice(&input[..first]);
                        let second = count - first;
                        if second != 0 {
                            state.bytes[..second].copy_from_slice(&input[first..count]);
                        }
                        state.length += count;
                    }
                    state.read_generation = crate::sync::next_readiness_generation();
                    PipeWrite::Bytes(count)
                }
            }
        };
        if matches!(result, PipeWrite::Bytes(_)) {
            self.notifier.notify(self);
        }
        result
    }

    fn close(self: &Arc<Self>, direction: PipeDirection) {
        {
            let mut state = self.state.lock();
            match direction {
                PipeDirection::Read => {
                    assert_ne!(state.readers, 0, "pipe reader underflow");
                    state.readers -= 1;
                    state.write_generation = crate::sync::next_readiness_generation();
                }
                PipeDirection::Write => {
                    assert_ne!(state.writers, 0, "pipe writer underflow");
                    state.writers -= 1;
                    state.read_generation = crate::sync::next_readiness_generation();
                }
            }
        }
        self.notifier.notify(self);
    }

    /// @description 发布一次合并的内核 readiness edge，并无条件通知 wait registry。
    ///
    /// @return 无返回值；Pipe 已无 reader 时幂等忽略。
    fn signal_readiness(self: &Arc<Self>) {
        let notify = {
            let mut state = self.state.lock();
            if state.readers == 0 {
                false
            } else {
                // token 只表示“至少有一次 edge”；每次 signal 仍推进 generation 并唤醒
                // registry。缺少无条件 wake 会使旧 token 压制新的、不同方向的 socket readiness。
                if state.length == 0 {
                    state.bytes[0] = 1;
                    state.head = 0;
                    state.length = 1;
                }
                state.read_generation = crate::sync::next_readiness_generation();
                true
            }
        };
        if notify {
            self.notifier.notify(self);
        }
    }

    /// @description 在 wait registry owner lock 内消费合并 readiness token，不反向通知同一 registry。
    ///
    /// @return 排空时观察到的 read generation；即使 token 已被其他 waiter 消费，
    /// generation 仍可证明某份更早的 snapshot 已失效。
    fn drain_readiness(self: &Arc<Self>) -> u64 {
        let mut state = self.state.lock();
        let generation = state.read_generation;
        if state.length != 0 {
            state.head = 0;
            state.length = 0;
            state.write_generation = crate::sync::next_readiness_generation();
        }
        generation
    }
}

/// @description 一个 OFD-owned anonymous pipe endpoint；dup/fork 共享同一 endpoint Arc。
pub(crate) struct PipeEnd {
    pipe: Arc<Pipe>,
    direction: PipeDirection,
}

impl PipeEnd {
    pub(crate) fn direction(&self) -> PipeDirection {
        self.direction
    }

    pub(crate) fn pipe(&self) -> Arc<Pipe> {
        self.pipe.clone()
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> PipeRead {
        self.pipe.read(output)
    }

    pub(crate) fn write(&self, input: &[u8]) -> PipeWrite {
        self.pipe.write(input, PipeWriteMode::Pipe)
    }

    /// @description 按 stream 语义写入当前可用容量，允许返回非零短写。
    ///
    /// @param input 待写入的连续字节。
    /// @return 写入字节数、无容量或 peer 已关闭。
    pub(crate) fn write_stream(&self, input: &[u8]) -> PipeWrite {
        self.pipe.write(input, PipeWriteMode::Stream)
    }

    /// @description 将本 Pipe 作为内核 readiness notification source 发布一次 edge。
    ///
    /// @return 无返回值；token 已存在时仍推进 generation 并通知 wait registry。
    /// @errors 只允许 write endpoint 调用，方向错误表示 kernel 装配不变量被破坏并 fail-stop。
    pub(crate) fn signal_readiness(&self) {
        assert_eq!(self.direction, PipeDirection::Write);
        self.pipe.signal_readiness();
    }

    /// @description 在内核 wait owner 临界区排空 readiness token，不消费任何 userspace data Pipe。
    ///
    /// @return 排空时观察到的 read generation；该值在 token 被消费后仍保持。
    /// @errors 只允许 read endpoint 调用，方向错误表示 kernel 装配不变量被破坏并 fail-stop。
    pub(crate) fn drain_readiness(&self) -> u64 {
        assert_eq!(self.direction, PipeDirection::Read);
        self.pipe.drain_readiness()
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        self.pipe.close(self.direction);
    }
}
