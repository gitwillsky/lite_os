use crate::sync::Mutex;
use alloc::{sync::Arc, sync::Weak, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use super::terminal::{Console, Terminal, clear_terminal_raw_input};
use crate::fs::FileSystemError;
use crate::ipc::{Pipe, PipeEnd, PipeRead, PipeWrite};
use fallible_tree::FallibleMap;

mod input_notification;

const PTY_INPUT_CAPACITY: usize = 4096;
const PTY_OUTPUT_ATOMIC_CAPACITY: usize = 512;

struct PtyConsoleState {
    input: [u8; PTY_INPUT_CAPACITY],
    head: usize,
    length: usize,
}

struct PtyConsole {
    pool: Arc<PtyPool>,
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

    fn discard_input(&self) -> usize {
        let count = {
            let mut state = self.state.lock();
            let PtyConsoleState { head, length, .. } = &mut *state;
            clear_terminal_raw_input(head, length)
        };
        if count != 0 {
            self.master_notification.signal_readiness();
        }
        count
    }

    fn discard_output(&self) -> usize {
        self.output.discard_buffered()
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

/// Unix98 PTY master/slave 共用的 line-discipline 与生命周期 owner。
pub(super) struct PtyPair {
    pub(super) index: u32,
    pub(super) inode: u64,
    pub(super) owner_uid: u32,
    pub(super) owner_gid: u32,
    console: Arc<PtyConsole>,
    terminal: Arc<Terminal>,
    slave_notification_read: Arc<PipeEnd>,
    slave_notification_write: Arc<PipeEnd>,
    master_notification_read: Arc<PipeEnd>,
    master_notification_write: Arc<PipeEnd>,
    state: Mutex<PtyState>,
}

/// `/dev/ptmx` OFD 的 raw master stream backend。
pub(crate) struct PtyMaster {
    pair: Arc<PtyPair>,
    output: Arc<PipeEnd>,
}

/// 一个已成功打开的 `/dev/pts/N` slave OFD 生命周期引用。
pub(crate) struct PtySlave {
    pair: Arc<PtyPair>,
    // OWNER: 此独立 slave open 的唯一 admission；close 与失败/最终 Drop 消费同一 token。
    registered: AtomicBool,
}

impl PtyMaster {
    /// 返回对应 `/dev/pts/N` index。
    pub(crate) fn index(&self) -> u32 {
        self.pair.index
    }

    /// 修改 slave lock；unlockpt 以零开放 slave pathname。
    ///
    /// # Parameters
    ///
    /// - `locked`: 非零 ioctl value 对应 true。
    pub(crate) fn set_locked(&self, locked: bool) {
        self.pair.state.lock().locked = locked;
    }

    /// 取得 slave 的唯一 Terminal owner，供 generic TTY ioctl 使用。
    pub(crate) fn terminal(&self) -> &Arc<Terminal> {
        &self.pair.terminal
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> PipeRead {
        let mut sink = crate::ipc::ReceiveBuffer::from_slice(output);
        self.output.read(&mut sink)
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
        let batch = self.pair.terminal.drain_input()?;
        let actions = input_notification::pty_input_actions(
            self.pair.terminal.input_ready(),
            batch.backlog,
            batch.signals,
        );
        if actions.signals != 0 {
            super::deliver_input_signals(&self.pair.terminal, actions.signals);
        }
        if actions.notify_slave {
            self.pair.slave_notification_write.signal_readiness();
        }
        Ok(count)
    }
}

impl PtyMaster {
    /// actual master fput 发布终态、关闭输出 reader，再在 pair guard 外交付 hangup。
    pub(crate) fn close(&self) {
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
            self.output.close();
            // 1. 先发布两端 readiness，确保 read/poll 不会永久睡眠。
            // 2. 再脱离 controlling session 并投递 SIGHUP/SIGCONT；缺失该顺序会让前台
            //    shell 在 master 关闭后仍持有一个永远不会产生输入的 controlling TTY。
            self.pair.slave_notification_write.signal_readiness();
            self.pair.master_notification_write.signal_readiness();
            super::hangup(&self.pair.terminal);
        }
    }
}

impl Drop for PtyMaster {
    fn drop(&mut self) {
        self.close();
    }
}

impl PtySlave {
    pub(crate) fn terminal(&self) -> &Arc<Terminal> {
        &self.pair.terminal
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.pair.slave_notification_read.pipe()
    }

    /// 返回 slave→master byte stream 的 write-side wait source。
    ///
    /// # Returns
    ///
    /// 与 `write` 使用同一容量/peer lifecycle owner 的 Pipe。
    pub(crate) fn output_pipe(&self) -> Arc<Pipe> {
        self.pair.console.output.pipe()
    }

    /// 查询 slave output 当前是否至少可原子提交一个 terminal chunk。
    ///
    /// # Returns
    ///
    /// master live 且 byte pipe 有空闲容量时为 true。
    pub(crate) fn output_writable(&self) -> bool {
        self.output_pipe()
            .poll_state(crate::ipc::PipeDirection::Write)
            .write_capacity
            >= PTY_OUTPUT_ATOMIC_CAPACITY
    }

    /// 计算一次 terminal retry 保证前进所需的保守 byte-pipe 容量。
    ///
    /// # Parameters
    ///
    /// - `input_length`: 尚未提交的 syscall staging bytes，范围为 1..=512。
    ///
    /// # Returns
    ///
    /// 覆盖 ONLCR 扩张且不超过单次 terminal atomic chunk 的等待容量。
    pub(crate) fn output_write_minimum(input_length: usize) -> usize {
        input_length
            .saturating_mul(2)
            .min(PTY_OUTPUT_ATOMIC_CAPACITY)
    }

    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.terminal().wait_ready() || self.master_hung_up() {
            return None;
        }
        self.pair.slave_notification_read.drain_readiness();
        (!self.terminal().wait_ready() && !self.master_hung_up()).then(|| self.notification_pipe())
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

impl PtySlave {
    /// actual slave fput 归还此 open；observer 与重复 Drop 不再次扣减 pair membership。
    pub(crate) fn close(&self) {
        if !self.registered.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut state = self.pair.state.lock();
        assert_ne!(state.slave_opens, 0, "PTY slave open count underflow");
        state.slave_opens -= 1;
        drop(state);
        self.pair.master_notification_write.signal_readiness();
    }
}
impl Drop for PtySlave {
    fn drop(&mut self) {
        self.close();
    }
}

struct PtyRegistry {
    slots: FallibleMap<u32, Weak<PtyPair>>,
    next_inode: u64,
}

/// weak allocation 观察者不占用索引；实际 master/slave admission 全部归还才允许复用。
fn reusable(slot: &Weak<PtyPair>) -> bool {
    slot.upgrade().is_none_or(|pair| {
        let state = pair.state.lock();
        !state.master_open && state.slave_opens == 0
    })
}

/// 一个 devpts 实例的 Unix98 index owner。
/// Weak slots 不保活已关闭端点；多个挂载可各自使用相同的可见 index。
pub(super) struct PtyPool {
    registry: Mutex<PtyRegistry>,
    limit: u32,
}

impl PtyPool {
    /// 创建空实例；分配失败返回 OutOfMemory。
    pub(super) fn new(limit: u32) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            registry: Mutex::new(PtyRegistry {
                slots: FallibleMap::new(),
                next_inode: 3,
            }),
            limit,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 分配新 Unix98 pair，并把 ptmx opener 记录为 slave inode owner。
    ///
    /// # Parameters
    ///
    /// - `owner_uid`: open `/dev/ptmx` 时的 effective UID。
    /// - `owner_gid`: open `/dev/ptmx` 时的 effective GID。
    ///
    /// # Returns
    ///
    /// master；Pipe、Terminal、Arc 或 registry storage OOM 返回错误。
    pub(super) fn open_master(
        self: &Arc<Self>,
        owner_uid: u32,
        owner_gid: u32,
    ) -> Result<Arc<PtyMaster>, FileSystemError> {
        let mut allocated =
            Arc::<PtyMaster>::try_new_uninit().map_err(|_| FileSystemError::OutOfMemory)?;
        let registry = &self.registry;
        let (output_read, output_write) =
            crate::ipc::Pipe::pair().map_err(|()| FileSystemError::OutOfMemory)?;
        let (slave_notification_read, slave_notification_write) =
            crate::ipc::Pipe::notification_pair().map_err(|()| FileSystemError::OutOfMemory)?;
        let (master_notification_read, master_notification_write) =
            crate::ipc::Pipe::notification_pair().map_err(|()| FileSystemError::OutOfMemory)?;
        let console = Arc::try_new(PtyConsole {
            pool: self.clone(),
            output: output_write,
            master_notification: master_notification_write.clone(),
            state: Mutex::new(PtyConsoleState {
                input: [0; PTY_INPUT_CAPACITY],
                head: 0,
                length: 0,
            }),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        // 1. 实际索引只有 registry 拥有；读取候选后在锁外准备完整 pair 与 AVL node。
        // 2. 发布时重验该候选。并发 opener 已占用则退休未发布对象并重试，不泄漏设备号。
        let pair = loop {
            let (index, inode) = {
                let registry = registry.lock();
                let mut candidate = 0;
                for (&index, slot) in registry.slots.iter() {
                    if index != candidate || reusable(slot) {
                        break;
                    }
                    candidate += 1;
                }
                if candidate >= self.limit {
                    return Err(FileSystemError::NoSpace);
                }
                (candidate, registry.next_inode)
            };
            let terminal = Terminal::new(
                console.clone(),
                crate::fs::DeviceNumber::new(super::PTS_MAJOR, index),
            )
            .map_err(|()| FileSystemError::OutOfMemory)?;
            let pair = Arc::try_new(PtyPair {
                index,
                inode,
                owner_uid,
                owner_gid,
                console: console.clone(),
                terminal,
                slave_notification_read: slave_notification_read.clone(),
                slave_notification_write: slave_notification_write.clone(),
                master_notification_read: master_notification_read.clone(),
                master_notification_write: master_notification_write.clone(),
                state: Mutex::new(PtyState {
                    locked: true,
                    master_open: true,
                    slave_opens: 0,
                }),
            })
            .map_err(|_| FileSystemError::OutOfMemory)?;
            let prepared = FallibleMap::try_prepare(index, Arc::downgrade(&pair))
                .map_err(|_| FileSystemError::OutOfMemory)?;
            let mut registry = registry.lock();
            if registry.next_inode != inode
                || registry
                    .slots
                    .get(&index)
                    .is_some_and(|slot| !reusable(slot))
            {
                drop(registry);
                drop(prepared);
                drop(pair);
                continue;
            }
            registry.next_inode = inode.checked_add(1).ok_or(FileSystemError::NoSpace)?;
            let retired = registry.slots.take_entry(&index);
            registry.slots.commit_vacant(prepared);
            drop(registry);
            drop(retired);
            break pair;
        };
        Arc::get_mut(&mut allocated)
            .expect("unpublished PTY master is unique")
            .write(PtyMaster {
                pair,
                output: output_read,
            });
        // SAFETY: 唯一 Arc storage 已完整初始化；publication 后没有 fallible allocation。
        Ok(unsafe { allocated.assume_init() })
    }

    /// 仅查找本实例内仍存活的 slave；不存在时返回 None。
    pub(super) fn pair(&self, index: u32) -> Option<Arc<PtyPair>> {
        self.registry
            .lock()
            .slots
            .get(&index)
            .and_then(Weak::upgrade)
            .filter(|pair| pair.state.lock().master_open)
    }

    /// 取得按可见 index 排序的 live inode 快照；分配失败返回 OutOfMemory。
    pub(super) fn live_indices(&self) -> Result<Vec<(u32, u64)>, FileSystemError> {
        let mut indices = Vec::new();
        loop {
            let count = self.registry.lock().slots.len();
            indices
                .try_reserve_exact(count)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            let registry = self.registry.lock();
            if indices.capacity() < registry.slots.len() {
                continue;
            }
            for (&index, slot) in registry.slots.iter() {
                if let Some(pair) = slot.upgrade().filter(|pair| pair.state.lock().master_open) {
                    indices.push((index, pair.inode));
                }
            }
            return Ok(indices);
        }
    }
}

impl PtyPair {
    /// 打开实际 pair 的 slave；已锁定或 master 已关闭返回 IoError。
    pub(super) fn open_slave(self: &Arc<Self>) -> Result<Arc<PtySlave>, FileSystemError> {
        let mut allocated =
            Arc::<PtySlave>::try_new_uninit().map_err(|_| FileSystemError::OutOfMemory)?;
        {
            let mut state = self.state.lock();
            if state.locked || !state.master_open {
                return Err(FileSystemError::IoError);
            }
            state.slave_opens = state
                .slave_opens
                .checked_add(1)
                .ok_or(FileSystemError::NoSpace)?;
        }
        Arc::get_mut(&mut allocated)
            .expect("unpublished PTY slave is unique")
            .write(PtySlave {
                pair: self.clone(),
                registered: AtomicBool::new(true),
            });
        // SAFETY: 完整初始化的唯一 storage；所有失败均发生在 slave admission 前。
        let slave = unsafe { allocated.assume_init() };
        self.master_notification_write.signal_readiness();
        Ok(slave)
    }
}

/// 重开 controlling PTY 的同一 pair。console 返回 None，已回收或编号复用返回 NoDevice。
pub(super) fn reopen_slave(
    terminal: &Arc<Terminal>,
) -> Result<Option<Arc<PtySlave>>, FileSystemError> {
    let Some(console) = (terminal.console() as &dyn core::any::Any).downcast_ref::<PtyConsole>()
    else {
        return Ok(None);
    };
    let pair = console
        .pool
        .pair(terminal.device_number().minor)
        .filter(|pair| Arc::ptr_eq(&pair.terminal, terminal))
        .ok_or(FileSystemError::NoDevice)?;
    pair.open_slave().map(Some)
}
