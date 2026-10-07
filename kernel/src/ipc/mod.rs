use crate::sync::WaitResult;
use alloc::{sync::Arc, vec::Vec};
use core::num::NonZeroUsize;
use spin::Mutex;

#[path = "receive_buffer.rs"]
mod receive_buffer;
pub(crate) use receive_buffer::ReceiveBuffer;

#[path = "eventfd.rs"]
mod eventfd;
pub(crate) use eventfd::{EventFd, EventFdRead, EventFdWrite};

pub(crate) const PIPE_BUF: usize = 4096;
/// `F_SETPIPE_SZ` 的上限（Linux `pipe-max-size` 默认值）；没有 CAP_SYS_RESOURCE 模型，所有调用者同限。
pub(crate) const PIPE_MAX_SIZE: usize = 1024 * 1024;

/// [`Pipe::resize`] 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeResizeError {
    /// 超过 [`PIPE_MAX_SIZE`]（`EPERM`）。
    TooLarge,
    /// 未读数据装不进新容量（`EBUSY`）。
    Busy,
    OutOfMemory,
}
const PIPE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
const NOTIFICATION_CAPACITY: NonZeroUsize = NonZeroUsize::MIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum PipeDirection {
    Read,
    Write,
}

/// blocking pipe I/O 的精确完成条件；写等待携带本次原子写所需的完整容量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeWaitCondition {
    Readable,
    Writable {
        minimum: usize,
    },
    /// 命名管道 open 汇合：对端自 `since` 之后又打开过一次。`waiter` 是等待者自己的方向。
    PeerOpened {
        waiter: PipeDirection,
        since: u64,
    },
}

impl PipeWaitCondition {
    /// 返回该 blocking condition 所属的 endpoint direction，并验证写容量范围。
    ///
    /// # Returns
    ///
    /// read/write endpoint direction；非法写容量破坏 kernel 调用契约并 fail-stop。
    pub(crate) fn direction(self) -> PipeDirection {
        match self {
            Self::Readable => PipeDirection::Read,
            Self::Writable { minimum } => {
                assert!((1..=PIPE_BUF).contains(&minimum));
                PipeDirection::Write
            }
            Self::PeerOpened { waiter, .. } => waiter,
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

/// byte ring 写入语义；匿名 pipe 保证 `PIPE_BUF` 原子性，stream socket 允许短写。
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
    /// read 侧环里有未读数据；与 `readable` 不同，EOF 不算数据（命名管道的 `POLLIN` 只看数据）。
    pub(crate) has_data: bool,
    /// 对端方向（read 侧看 writer、write 侧看 reader）累计被打开的次数；只有命名管道会增长。
    pub(crate) peer_opens: u64,
    /// 有任务正在命名管道 open 汇合里等待对端；唤醒路径据此在没有 poll 事件时仍检查等待者。
    pub(crate) rendezvous: bool,
}

impl PipePollState {
    /// 判断同一 PipeState snapshot 是否满足 blocking I/O 的精确完成条件。
    ///
    /// # Parameters
    ///
    /// - `condition`: read data/EOF 或一笔 `PIPE_BUF` 范围内原子写所需的完整容量。
    ///
    /// # Returns
    ///
    /// 条件已满足或 write endpoint 已 broken 时返回 true。
    pub(crate) fn satisfies(self, condition: PipeWaitCondition) -> bool {
        match condition {
            PipeWaitCondition::Readable => self.readable,
            PipeWaitCondition::Writable { minimum } => self.error || self.write_capacity >= minimum,
            PipeWaitCondition::PeerOpened { since, .. } => self.peer_opens > since,
        }
    }
}

/// scheduler 为 Pipe 提供的唤醒与阻塞实现；全内核只安装一份。
pub(crate) trait PipeScheduler: Send + Sync {
    /// Pipe 状态变为可读、可写、EOF 或 broken 时唤醒等待者与 poller。
    fn notify(&self, pipe: &Arc<Pipe>);

    /// 阻塞当前 task 直到 `condition` 成立、`deadline` 到期或被可交付 signal 中断。
    fn wait(
        &self,
        pipe: &Arc<Pipe>,
        condition: PipeWaitCondition,
        deadline: Option<u64>,
    ) -> WaitResult;
}

// OWNER: task 初始化时安装的唯一 Pipe scheduler。全部 Pipe 共用它，任何层都能直接创建并阻塞
// 等待 Pipe 而无需依赖 task；缺失时状态变化无法唤醒阻塞的 reader/writer/poller。
static PIPE_SCHEDULER: spin::Once<&'static dyn PipeScheduler> = spin::Once::new();

/// 安装 Pipe 的唯一 scheduler。
///
/// # Panics
///
/// 重复安装时 panic。
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn install_pipe_scheduler(scheduler: &'static dyn PipeScheduler) {
    assert!(
        PIPE_SCHEDULER.get().is_none(),
        "pipe scheduler installed twice"
    );
    PIPE_SCHEDULER.call_once(|| scheduler);
}

/// 发布一次 Pipe 状态变化。
///
/// scheduler 安装之前不存在任何 task，因此也不存在可唤醒的 waiter 或 poller；
/// 启动期子系统在该窗口内创建并 signal 的 Pipe 由首个 waiter 在登记后复查状态观察到。
fn publish_state_change(pipe: &Arc<Pipe>) {
    if let Some(scheduler) = PIPE_SCHEDULER.get() {
        scheduler.notify(pipe);
    }
}

struct PipeState {
    bytes: Vec<u8>,
    head: usize,
    length: usize,
    readers: usize,
    writers: usize,
    // 命名管道的 reader/writer 累计打开次数。open 汇合等待“自我开始等待之后对端又打开过”，而不是
    // “此刻对端存在”：对端打开后立即关闭也必须放行等待者（Linux `r_counter`/`w_counter`）。
    reader_opens: u64,
    writer_opens: u64,
    rendezvous_waiters: usize,
    read_generation: u64,
    write_generation: u64,
}

/// data/notification Pipe 的唯一 byte ring、generation 与 endpoint lifecycle owner。
pub(crate) struct Pipe {
    // Pipe owner 分配一次并由两个 endpoint 共享；缺失时 read/write fd 会报告不同 pipe inode。
    object_id: u64,
    state: Mutex<PipeState>,
}

impl Pipe {
    /// 创建一对唯一 read/write endpoint。
    ///
    /// # Parameters
    ///
    /// - `notifier`: 在状态变为可读、可写、EOF 或 broken 时唤醒 task registry。
    ///
    /// # Returns
    ///
    /// 阻塞当前 task 直到 `condition` 成立、`deadline` 到期或被可交付 signal 中断。
    ///
    /// # Parameters
    ///
    /// - `condition`: read data/EOF 或写入所需容量/broken peer。
    /// - `deadline`: 可选 absolute monotonic 纳秒 deadline。
    ///
    /// # Panics
    ///
    /// scheduler 尚未安装（不在 task context）时 panic。
    pub(crate) fn wait(
        self: &Arc<Self>,
        condition: PipeWaitCondition,
        deadline: Option<u64>,
    ) -> WaitResult {
        PIPE_SCHEDULER
            .get()
            .expect("pipe wait requires the installed scheduler")
            .wait(self, condition, deadline)
    }

    /// 两个 endpoint；kernel heap 不足返回错误。
    pub(crate) fn pair() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
        Self::pair_with_capacity(PIPE_CAPACITY)
    }

    /// 创建只承载合并 readiness token 的一字节 Pipe endpoints。
    ///
    /// # Parameters
    ///
    /// - `notifier`: 与 data Pipe 共用的 task wait-registry 通知 seam。
    ///
    /// # Returns
    ///
    /// 两个 endpoint；kernel heap 不足返回错误。
    pub(crate) fn notification_pair() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
        Self::pair_with_capacity(NOTIFICATION_CAPACITY)
    }

    fn pair_with_capacity(capacity: NonZeroUsize) -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
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
                reader_opens: 1,
                writer_opens: 1,
                rendezvous_waiters: 0,
                read_generation: crate::sync::next_readiness_generation(),
                write_generation: crate::sync::next_readiness_generation(),
            }),
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

    /// 创建没有任何 endpoint 的命名管道（FIFO）实体；endpoint 由 [`Self::open_end`] 逐个打开。
    ///
    /// # Returns
    ///
    /// 初始 reader/writer 计数为零的 Pipe；heap 不足返回错误。
    pub(crate) fn new_named() -> Result<Arc<Self>, ()> {
        let capacity = PIPE_CAPACITY.get();
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| ())?;
        bytes.resize(capacity, 0);
        Arc::try_new(Self {
            object_id: crate::id::next_runtime_object_id(),
            state: Mutex::new(PipeState {
                bytes,
                head: 0,
                length: 0,
                readers: 0,
                writers: 0,
                reader_opens: 0,
                writer_opens: 0,
                rendezvous_waiters: 0,
                read_generation: crate::sync::next_readiness_generation(),
                write_generation: crate::sync::next_readiness_generation(),
            }),
        })
        .map_err(|_| ())
    }

    /// 打开一个新的 endpoint 并计数；已有 endpoint 的 Pipe 上可重复调用（FIFO 的多次 open）。
    ///
    /// # Returns
    ///
    /// endpoint；Drop 时归还计数。heap 不足返回错误且不改变计数。
    pub(crate) fn open_end(self: &Arc<Self>, direction: PipeDirection) -> Result<Arc<PipeEnd>, ()> {
        // 先分配 endpoint：之后的计数与发布不会失败，计数变化因此总有对应的 Drop。
        let end = Arc::try_new(PipeEnd {
            pipe: self.clone(),
            direction,
        })
        .map_err(|_| ())?;
        {
            let mut state = self.state.lock();
            match direction {
                PipeDirection::Read => {
                    state.readers += 1;
                    state.reader_opens += 1;
                    state.write_generation = crate::sync::next_readiness_generation();
                }
                PipeDirection::Write => {
                    state.writers += 1;
                    state.writer_opens += 1;
                    state.read_generation = crate::sync::next_readiness_generation();
                }
            }
        }
        publish_state_change(self);
        Ok(end)
    }

    /// 当前对端存在的 endpoint 数与对端累计打开次数（命名管道 open 汇合的起点快照）。
    ///
    /// # Parameters
    ///
    /// - `waiter`: 调用者自己的方向。
    pub(crate) fn peer_snapshot(&self, waiter: PipeDirection) -> (usize, u64) {
        let state = self.state.lock();
        match waiter {
            PipeDirection::Read => (state.writers, state.writer_opens),
            PipeDirection::Write => (state.readers, state.reader_opens),
        }
    }

    /// 阻塞到对端自 `since` 之后又打开过一次，或被 signal 中断。
    ///
    /// 等待期间登记为汇合等待者，使对端的 open 能走到唤醒路径；返回前一定撤销登记。
    pub(crate) fn wait_for_peer(self: &Arc<Self>, waiter: PipeDirection, since: u64) -> WaitResult {
        self.state.lock().rendezvous_waiters += 1;
        let result = self.wait(PipeWaitCondition::PeerOpened { waiter, since }, None);
        let others_waiting = {
            let mut state = self.state.lock();
            state.rendezvous_waiters -= 1;
            state.rendezvous_waiters != 0
        };
        if others_waiting {
            // 直接管道等待是 wake-one：一次对端到来只唤醒一个等待者，但对端到来不消耗任何东西，
            // 所有同时阻塞的等待者都该放行。每个醒来的等待者接力再发布一次状态变化，由下一个等待者
            // 继续（条件不满足的会被跳过），链式唤醒不需要放宽 wake-one 不变量。
            publish_state_change(self);
        }
        result
    }

    /// 环里尚未读取的字节数（`FIONREAD`）。
    pub(crate) fn buffered_bytes(&self) -> usize {
        self.state.lock().length
    }

    /// 环的容量（`F_GETPIPE_SZ`）。
    pub(crate) fn capacity(&self) -> usize {
        self.state.lock().bytes.len()
    }

    /// 把环容量改为不小于 `requested` 的最小 2 的幂次页数（`F_SETPIPE_SZ`）。
    ///
    /// 新环在锁外分配，然后在锁内把未读数据线性化搬过去；失败时旧环原封不动。
    ///
    /// # Returns
    ///
    /// 新的容量。
    ///
    /// # Errors
    ///
    /// 超过 [`PIPE_MAX_SIZE`] 返回 `TooLarge`；未读数据多于新容量返回 `Busy`；分配失败返回
    /// `OutOfMemory`。
    pub(crate) fn resize(self: &Arc<Self>, requested: usize) -> Result<usize, PipeResizeError> {
        if requested > PIPE_MAX_SIZE {
            return Err(PipeResizeError::TooLarge);
        }
        // 至少一页；`PIPE_BUF` 原子写要求容量不小于一页。
        let capacity = requested.max(PIPE_BUF).next_power_of_two();
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| PipeResizeError::OutOfMemory)?;
        bytes.resize(capacity, 0);
        {
            let mut state = self.state.lock();
            if state.length > capacity {
                return Err(PipeResizeError::Busy);
            }
            let old = state.bytes.len();
            let first = state.length.min(old - state.head);
            bytes[..first].copy_from_slice(&state.bytes[state.head..state.head + first]);
            let rest = state.length - first;
            bytes[first..first + rest].copy_from_slice(&state.bytes[..rest]);
            state.bytes = bytes;
            state.head = 0;
            // 容量变化可能让阻塞的 writer 立即可写。
            state.write_generation = crate::sync::next_readiness_generation();
        }
        publish_state_change(self);
        Ok(capacity)
    }

    pub(crate) fn identity(pipe: &Arc<Self>) -> usize {
        Arc::as_ptr(pipe) as usize
    }

    pub(crate) fn object_id(&self) -> u64 {
        self.object_id
    }

    /// 在 Pipe owner lock 下复查 blocking I/O 的精确完成条件。
    ///
    /// # Parameters
    ///
    /// - `condition`: read data/EOF 或一笔原子写所需的完整容量。
    ///
    /// # Returns
    ///
    /// 当前状态满足条件时返回 true。
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
                has_data: state.length != 0,
                peer_opens: state.writer_opens,
                rendezvous: state.rendezvous_waiters != 0,
            },
            PipeDirection::Write => PipePollState {
                readable: false,
                writable: state.readers != 0 && state.length != state.bytes.len(),
                hangup: false,
                error: state.readers == 0,
                write_capacity: state.bytes.len() - state.length,
                has_data: false,
                peer_opens: state.reader_opens,
                rendezvous: state.rendezvous_waiters != 0,
            },
        }
    }

    /// 返回指定 endpoint 最近一次可观察状态变化的全局 generation。
    ///
    /// # Parameters
    ///
    /// - `direction`: read 侧跟踪 data/EOF，write 侧跟踪 space/broken-pipe。
    ///
    /// # Returns
    ///
    /// 跨 I/O source 可比较的 generation。
    pub(crate) fn readiness_generation(&self, direction: PipeDirection) -> u64 {
        let state = self.state.lock();
        match direction {
            PipeDirection::Read => state.read_generation,
            PipeDirection::Write => state.write_generation,
        }
    }

    fn read(self: &Arc<Self>, output: &mut ReceiveBuffer<'_>, maximum: usize) -> PipeRead {
        let result = {
            let mut state = self.state.lock();
            if state.length == 0 {
                if state.writers == 0 {
                    PipeRead::Eof
                } else {
                    PipeRead::Empty
                }
            } else {
                let count = output.remaining().min(maximum).min(state.length);
                if count != 0 {
                    let capacity = state.bytes.len();
                    let head = state.head;
                    let first = count.min(capacity - head);
                    assert_eq!(output.append(&state.bytes[head..head + first]), first);
                    let second = count - first;
                    if second != 0 {
                        assert_eq!(output.append(&state.bytes[..second]), second);
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
            publish_state_change(self);
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
            publish_state_change(self);
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
        publish_state_change(self);
    }

    /// 发布一次合并的内核 readiness edge，并无条件通知 wait registry。
    ///
    /// # Returns
    ///
    /// 无返回值；Pipe 已无 reader 时幂等忽略。
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
            publish_state_change(self);
        }
    }

    /// 在 wait registry owner lock 内消费合并 readiness token，不反向通知同一 registry。
    ///
    /// # Returns
    ///
    /// 排空时观察到的 read generation；即使 token 已被其他 waiter 消费，
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

    /// 丢弃 byte ring 中尚未由 reader 消费的全部数据。
    ///
    /// # Returns
    ///
    /// 被丢弃的 byte 数；read/write readiness generation 均已推进。
    fn discard_buffered(self: &Arc<Self>) -> usize {
        let discarded = {
            let mut state = self.state.lock();
            let discarded = state.length;
            if discarded != 0 {
                state.head = 0;
                state.length = 0;
                state.read_generation = crate::sync::next_readiness_generation();
                state.write_generation = crate::sync::next_readiness_generation();
            }
            discarded
        };
        if discarded != 0 {
            publish_state_change(self);
        }
        discarded
    }
}

/// 一个 OFD-owned anonymous pipe endpoint；dup/fork 共享同一 endpoint Arc。
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

    pub(crate) fn read(&self, output: &mut ReceiveBuffer<'_>) -> PipeRead {
        let maximum = output.remaining();
        self.pipe.read(output, maximum)
    }

    /// 从 pipe 读取至 receive sink，但不越过 protocol/control barrier。
    ///
    /// # Parameters
    ///
    /// - `output`: initialized-prefix owner。
    /// - `maximum`: 本次最多追加的 byte count。
    ///
    /// # Returns
    ///
    /// byte count、empty 或 EOF。
    pub(crate) fn read_bounded(&self, output: &mut ReceiveBuffer<'_>, maximum: usize) -> PipeRead {
        self.pipe.read(output, maximum)
    }

    pub(crate) fn write(&self, input: &[u8]) -> PipeWrite {
        self.pipe.write(input, PipeWriteMode::Pipe)
    }

    /// 按 stream 语义写入当前可用容量，允许返回非零短写。
    ///
    /// # Parameters
    ///
    /// - `input`: 待写入的连续字节。
    ///
    /// # Returns
    ///
    /// 写入字节数、无容量或 peer 已关闭。
    pub(crate) fn write_stream(&self, input: &[u8]) -> PipeWrite {
        self.pipe.write(input, PipeWriteMode::Stream)
    }

    /// 将本 Pipe 作为内核 readiness notification source 发布一次 edge。
    ///
    /// # Returns
    ///
    /// 无返回值；token 已存在时仍推进 generation 并通知 wait registry。
    ///
    /// # Errors
    ///
    /// 只允许 write endpoint 调用，方向错误表示 kernel 装配不变量被破坏并 fail-stop。
    pub(crate) fn signal_readiness(&self) {
        assert_eq!(self.direction, PipeDirection::Write);
        self.pipe.signal_readiness();
    }

    /// 在内核 wait owner 临界区排空 readiness token，不消费任何 userspace data Pipe。
    ///
    /// # Returns
    ///
    /// 排空时观察到的 read generation；该值在 token 被消费后仍保持。
    ///
    /// # Errors
    ///
    /// 只允许 read endpoint 调用，方向错误表示 kernel 装配不变量被破坏并 fail-stop。
    pub(crate) fn drain_readiness(&self) -> u64 {
        assert_eq!(self.direction, PipeDirection::Read);
        self.pipe.drain_readiness()
    }

    /// 丢弃 write endpoint 已发布、read endpoint 尚未消费的全部 bytes。
    ///
    /// # Returns
    ///
    /// 被丢弃的 byte 数。
    pub(crate) fn discard_buffered(&self) -> usize {
        assert_eq!(self.direction, PipeDirection::Write);
        self.pipe.discard_buffered()
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        self.pipe.close(self.direction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_flush_discards_unread_pipe_bytes() {
        let (read, write) = Pipe::pair().expect("pipe");
        assert_eq!(write.write(b"pending"), PipeWrite::Bytes(7));

        assert_eq!(write.discard_buffered(), 7);
        assert_eq!(write.discard_buffered(), 0);

        let mut storage = [0u8; 8];
        let mut output = ReceiveBuffer::from_slice(&mut storage);
        assert_eq!(read.read(&mut output), PipeRead::Empty);
        assert_eq!(
            write.pipe.poll_state(PipeDirection::Write).write_capacity,
            PIPE_CAPACITY.get()
        );
    }
}
