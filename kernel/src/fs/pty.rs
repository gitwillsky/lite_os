use alloc::{sync::Arc, sync::Weak, vec::Vec};
use spin::{Mutex, Once};

use super::{Console, FileSystemError, Terminal};
use crate::ipc::{Pipe, PipeEnd, PipeRead, PipeWrite};

type PipeFactory = fn() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()>;
type HangupNotifier = fn(&Terminal);
const PTY_INPUT_CAPACITY: usize = 4096;
const PTY_OUTPUT_ATOMIC_CAPACITY: usize = 512;

struct PtyConsoleState {
    input: [u8; PTY_INPUT_CAPACITY],
    head: usize,
    length: usize,
}

struct PtyConsole {
    output: Arc<PipeEnd>,
    master_notification: Arc<PipeEnd>,
    state: Mutex<PtyConsoleState>,
}

impl PtyConsole {
    fn push_input(&self, bytes: &[u8]) -> usize {
        let mut state = self.state.lock();
        let count = bytes.len().min(PTY_INPUT_CAPACITY - state.length);
        for byte in bytes.iter().take(count) {
            let tail = (state.head + state.length) % state.input.len();
            state.input[tail] = *byte;
            state.length += 1;
        }
        count
    }

    fn input_writable(&self) -> bool {
        self.state.lock().length != PTY_INPUT_CAPACITY
    }
}

impl Console for PtyConsole {
    fn read(&self, bytes: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut state = self.state.lock();
        let count = bytes.len().min(state.length);
        for byte in bytes.iter_mut().take(count) {
            *byte = state.input[state.head];
            state.head = (state.head + 1) % state.input.len();
            state.length -= 1;
        }
        drop(state);
        if count != 0 {
            self.master_notification.signal_readiness();
        }
        Ok(count)
    }

    fn input_ready(&self) -> bool {
        self.state.lock().length != 0
    }

    fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError> {
        match self.output.write(bytes) {
            PipeWrite::Bytes(count) => {
                self.master_notification.signal_readiness();
                Ok(count)
            }
            PipeWrite::Full => Ok(0),
            PipeWrite::Broken => Err(FileSystemError::IoError),
        }
    }
}

struct PtyState {
    locked: bool,
    master_open: bool,
    slave_opens: usize,
}

/// @description Unix98 PTY master/slave 共用的 line-discipline 与生命周期 owner。
pub(crate) struct PtyPair {
    index: u32,
    console: Arc<PtyConsole>,
    terminal: Arc<Terminal>,
    slave_notification_read: Arc<PipeEnd>,
    slave_notification_write: Arc<PipeEnd>,
    master_notification_read: Arc<PipeEnd>,
    master_notification_write: Arc<PipeEnd>,
    hangup: HangupNotifier,
    state: Mutex<PtyState>,
}

/// @description `/dev/ptmx` OFD 的 raw master stream backend。
pub(crate) struct PtyMaster {
    pair: Arc<PtyPair>,
    output: Arc<PipeEnd>,
}

/// @description 一个已成功打开的 `/dev/pts/N` slave OFD 生命周期引用。
pub(crate) struct PtySlave {
    pair: Arc<PtyPair>,
}

impl PtyMaster {
    /// @description 返回对应 `/dev/pts/N` index。
    pub(crate) fn index(&self) -> u32 {
        self.pair.index
    }

    /// @description 修改 slave lock；unlockpt 以零开放 slave pathname。
    /// @param locked 非零 ioctl value 对应 true。
    pub(crate) fn set_locked(&self, locked: bool) {
        self.pair.state.lock().locked = locked;
    }

    /// @description 取得 slave 的唯一 Terminal owner，供 generic TTY ioctl 使用。
    pub(crate) fn terminal(&self) -> &Arc<Terminal> {
        &self.pair.terminal
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> PipeRead {
        self.output.read(output)
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.pair.master_notification_read.pipe()
    }

    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.readable() || self.peer_hung_up() {
            return None;
        }
        self.pair.master_notification_read.drain_readiness();
        (!self.readable() && !self.peer_hung_up()).then(|| self.notification_pipe())
    }

    pub(crate) fn prepare_write_to_block(&self) -> Option<Arc<Pipe>> {
        if self.writable() || self.peer_hung_up() {
            return None;
        }
        self.pair.master_notification_read.drain_readiness();
        (!self.writable() && !self.peer_hung_up()).then(|| self.notification_pipe())
    }

    pub(crate) fn readable(&self) -> bool {
        self.output
            .pipe()
            .poll_state(crate::ipc::PipeDirection::Read)
            .readable
    }

    pub(crate) fn peer_hung_up(&self) -> bool {
        self.pair.state.lock().slave_opens == 0
    }

    pub(crate) fn writable(&self) -> bool {
        self.pair.console.input_writable()
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, FileSystemError> {
        if self.peer_hung_up() {
            return Err(FileSystemError::IoError);
        }
        let count = self.pair.console.push_input(input);
        if count == 0 {
            return Ok(0);
        }
        self.pair.terminal.drain_input()?;
        if self.pair.terminal.input_ready() {
            self.pair.slave_notification_write.signal_readiness();
        }
        Ok(count)
    }
}

impl Drop for PtyMaster {
    fn drop(&mut self) {
        let notify_hangup = {
            let mut state = self.pair.state.lock();
            if !state.master_open {
                false
            } else {
                state.master_open = false;
                true
            }
        };
        if notify_hangup {
            // 1. 先发布两端 readiness，确保 read/poll 不会永久睡眠。
            // 2. 再脱离 controlling session 并投递 SIGHUP/SIGCONT；缺失该顺序会让前台
            //    shell 在 master 关闭后仍持有一个永远不会产生输入的 controlling TTY。
            self.pair.slave_notification_write.signal_readiness();
            self.pair.master_notification_write.signal_readiness();
            (self.pair.hangup)(&self.pair.terminal);
        }
    }
}

impl PtySlave {
    pub(crate) fn terminal(&self) -> &Arc<Terminal> {
        &self.pair.terminal
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.pair.slave_notification_read.pipe()
    }

    /// @description 返回 slave→master byte stream 的 write-side wait source。
    /// @return 与 `write` 使用同一容量/peer lifecycle owner 的 Pipe。
    pub(crate) fn output_pipe(&self) -> Arc<Pipe> {
        self.pair.console.output.pipe()
    }

    /// @description 查询 slave output 当前是否至少可原子提交一个 terminal chunk。
    /// @return master live 且 byte pipe 有空闲容量时为 true。
    pub(crate) fn output_writable(&self) -> bool {
        self.output_pipe()
            .poll_state(crate::ipc::PipeDirection::Write)
            .write_capacity
            >= PTY_OUTPUT_ATOMIC_CAPACITY
    }

    /// @description 计算一次 terminal retry 保证前进所需的保守 byte-pipe 容量。
    /// @param input_length 尚未提交的 syscall staging bytes，范围为 1..=512。
    /// @return 覆盖 ONLCR 扩张且不超过单次 terminal atomic chunk 的等待容量。
    pub(crate) fn output_write_minimum(input_length: usize) -> usize {
        input_length
            .saturating_mul(2)
            .min(PTY_OUTPUT_ATOMIC_CAPACITY)
    }

    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.terminal().input_ready() || self.master_hung_up() {
            return None;
        }
        self.pair.slave_notification_read.drain_readiness();
        (!self.terminal().input_ready() && !self.master_hung_up()).then(|| self.notification_pipe())
    }

    pub(crate) fn master_hung_up(&self) -> bool {
        !self.pair.state.lock().master_open
    }

    pub(crate) fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError> {
        if self.master_hung_up() {
            return Err(FileSystemError::IoError);
        }
        self.terminal().write(bytes)
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        self.notification_pipe()
            .readiness_generation(crate::ipc::PipeDirection::Read)
            .max(
                self.output_pipe()
                    .readiness_generation(crate::ipc::PipeDirection::Write),
            )
    }
}

impl Drop for PtySlave {
    fn drop(&mut self) {
        let mut state = self.pair.state.lock();
        assert_ne!(state.slave_opens, 0, "PTY slave open count underflow");
        state.slave_opens -= 1;
        drop(state);
        self.pair.master_notification_write.signal_readiness();
    }
}

struct PtyRegistry {
    pipes: PipeFactories,
    hangup: HangupNotifier,
    slots: Vec<Weak<PtyPair>>,
}

#[derive(Clone, Copy)]
struct PipeFactories {
    data: PipeFactory,
    notification: PipeFactory,
}

// OWNER: pty module 唯一拥有 Unix98 index namespace、生命周期和 transport factory。weak
// slots 允许最后一个 endpoint 关闭后原位复用；缺失此 registry 会让 devpts 与 ptmx pair 分裂。
static PTYS: Once<Mutex<PtyRegistry>> = Once::new();

/// @description 装配 Unix98 PTY transport 与 controlling-terminal hangup seam。
/// @param data_factory composition root 提供的 64 KiB output Pipe constructor。
/// @param notification_factory composition root 提供的一字节 readiness Pipe constructor。
/// @param hangup task owner 提供的无分配 SIGHUP/SIGCONT notifier。
/// @return 首次初始化成功；重复初始化返回错误。
pub(crate) fn init(
    data_factory: PipeFactory,
    notification_factory: PipeFactory,
    hangup: HangupNotifier,
) -> Result<(), ()> {
    if PTYS.get().is_some() {
        return Err(());
    }
    PTYS.call_once(|| {
        Mutex::new(PtyRegistry {
            pipes: PipeFactories {
                data: data_factory,
                notification: notification_factory,
            },
            hangup,
            slots: Vec::new(),
        })
    });
    Ok(())
}

/// @description 分配新 Unix98 pair 并返回 ptmx master backend。
/// @return master；Pipe、Terminal、Arc 或 registry storage OOM 返回错误。
pub(crate) fn open_master() -> Result<Arc<PtyMaster>, FileSystemError> {
    let registry = PTYS.get().ok_or(FileSystemError::InvalidOperation)?;
    let (pipes, hangup) = {
        let registry = registry.lock();
        (registry.pipes, registry.hangup)
    };
    let (output_read, output_write) = (pipes.data)().map_err(|()| FileSystemError::OutOfMemory)?;
    let (slave_notification_read, slave_notification_write) =
        (pipes.notification)().map_err(|()| FileSystemError::OutOfMemory)?;
    let (master_notification_read, master_notification_write) =
        (pipes.notification)().map_err(|()| FileSystemError::OutOfMemory)?;
    let console = Arc::try_new(PtyConsole {
        output: output_write,
        master_notification: master_notification_write.clone(),
        state: Mutex::new(PtyConsoleState {
            input: [0; PTY_INPUT_CAPACITY],
            head: 0,
            length: 0,
        }),
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    let terminal = Terminal::new(console.clone()).map_err(|()| FileSystemError::OutOfMemory)?;

    let mut registry = registry.lock();
    let index = if let Some(index) = registry
        .slots
        .iter()
        .position(|slot| slot.upgrade().is_none())
    {
        u32::try_from(index).map_err(|_| FileSystemError::NoSpace)?
    } else {
        registry
            .slots
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        u32::try_from(registry.slots.len()).map_err(|_| FileSystemError::NoSpace)?
    };
    let pair = Arc::try_new(PtyPair {
        index,
        console,
        terminal,
        slave_notification_read,
        slave_notification_write,
        master_notification_read,
        master_notification_write,
        hangup,
        state: Mutex::new(PtyState {
            locked: true,
            master_open: true,
            slave_opens: 0,
        }),
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    if index as usize == registry.slots.len() {
        registry.slots.push(Arc::downgrade(&pair));
    } else {
        registry.slots[index as usize] = Arc::downgrade(&pair);
    }
    drop(registry);
    Arc::try_new(PtyMaster {
        pair,
        output: output_read,
    })
    .map_err(|_| FileSystemError::OutOfMemory)
}

/// @description 打开 live 且已 unlock 的 `/dev/pts/N` slave。
/// @param index TIOCGPTN 返回的 device index。
/// @return 独立 slave open 引用；不存在返回 NotFound，锁定或 master 已关闭返回 IoError。
pub(crate) fn open_slave(index: u32) -> Result<Arc<PtySlave>, FileSystemError> {
    let pair = PTYS
        .get()
        .ok_or(FileSystemError::InvalidOperation)?
        .lock()
        .slots
        .get(index as usize)
        .and_then(Weak::upgrade)
        .ok_or(FileSystemError::NotFound)?;
    {
        let mut state = pair.state.lock();
        if state.locked || !state.master_open {
            return Err(FileSystemError::IoError);
        }
        state.slave_opens = state
            .slave_opens
            .checked_add(1)
            .ok_or(FileSystemError::NoSpace)?;
    }
    let slave = Arc::try_new(PtySlave { pair: pair.clone() }).map_err(|_| {
        pair.state.lock().slave_opens -= 1;
        FileSystemError::OutOfMemory
    })?;
    pair.master_notification_write.signal_readiness();
    Ok(slave)
}

pub(crate) fn slave_exists(index: u32) -> bool {
    PTYS.get().is_some_and(|registry| {
        registry
            .lock()
            .slots
            .get(index as usize)
            .and_then(Weak::upgrade)
            .is_some_and(|pair| pair.state.lock().master_open)
    })
}

/// @description 为 devpts getdents 取得当前 live master index 快照。
/// @return 升序 index；快照 storage OOM 返回错误。
pub(crate) fn slave_indices() -> Result<Vec<u32>, FileSystemError> {
    let registry = PTYS.get().ok_or(FileSystemError::InvalidOperation)?.lock();
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(registry.slots.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for (index, slot) in registry.slots.iter().enumerate() {
        if slot
            .upgrade()
            .is_some_and(|pair| pair.state.lock().master_open)
        {
            indices.push(u32::try_from(index).map_err(|_| FileSystemError::NoSpace)?);
        }
    }
    Ok(indices)
}
