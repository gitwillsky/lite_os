use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

pub(crate) const PIPE_BUF: usize = 4096;
const PIPE_CAPACITY: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum PipeDirection {
    Read,
    Write,
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct PipePollState {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    pub(crate) hangup: bool,
    pub(crate) error: bool,
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

/// @description anonymous pipe 的唯一 byte ring 与 endpoint lifecycle owner。
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
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(PIPE_CAPACITY).map_err(|_| ())?;
        bytes.resize(PIPE_CAPACITY, 0);
        let pipe = Arc::new(Self {
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
        });
        Ok((
            Arc::new(PipeEnd {
                pipe: pipe.clone(),
                direction: PipeDirection::Read,
            }),
            Arc::new(PipeEnd {
                pipe,
                direction: PipeDirection::Write,
            }),
        ))
    }

    pub(crate) fn identity(pipe: &Arc<Self>) -> usize {
        Arc::as_ptr(pipe) as usize
    }

    pub(crate) fn object_id(&self) -> u64 {
        self.object_id
    }

    pub(crate) fn readable(&self) -> bool {
        let state = self.state.lock();
        state.length != 0 || state.writers == 0
    }

    pub(crate) fn writable(&self) -> bool {
        let state = self.state.lock();
        state.readers == 0 || state.length != state.bytes.len()
    }

    pub(crate) fn poll_state(&self, direction: PipeDirection) -> PipePollState {
        let state = self.state.lock();
        match direction {
            PipeDirection::Read => PipePollState {
                readable: state.length != 0 || state.writers == 0,
                writable: false,
                hangup: state.writers == 0,
                error: false,
            },
            PipeDirection::Write => PipePollState {
                readable: false,
                writable: state.readers != 0 && state.length != state.bytes.len(),
                hangup: false,
                error: state.readers == 0,
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
                for byte in output.iter_mut().take(count) {
                    *byte = state.bytes[state.head];
                    state.head = (state.head + 1) % state.bytes.len();
                    state.length -= 1;
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

    fn write(self: &Arc<Self>, input: &[u8]) -> PipeWrite {
        let result = {
            let mut state = self.state.lock();
            if state.readers == 0 {
                PipeWrite::Broken
            } else {
                let available = state.bytes.len() - state.length;
                if available == 0 || input.len() <= PIPE_BUF && available < input.len() {
                    PipeWrite::Full
                } else {
                    let count = available.min(input.len());
                    for byte in input.iter().take(count) {
                        let tail = (state.head + state.length) % state.bytes.len();
                        state.bytes[tail] = *byte;
                        state.length += 1;
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
        self.pipe.write(input)
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        self.pipe.close(self.direction);
    }
}
