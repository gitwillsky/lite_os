//! 一个挂载的 FUSE 连接（Linux `fuse_conn` + `fuse_iqueue` + 每个设备的 `fuse_pqueue`）。
//!
//! 请求生命周期：
//! 1. 调用者等 INIT 完成后入 `pending`，唤醒守护进程；
//! 2. 守护进程 read 取走请求，移入读取它的设备的 `processing`，状态变为 Sent；
//! 3. 同一设备 write 回复，按 unique 匹配后完成请求并唤醒调用者；
//! 4. 中止（最后一个设备关闭或卸载）以 `ENOTCONN`/`ECONNABORTED` 完成全部未完成请求。
use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use spin::{Mutex, Once};
use syscall_abi::{errno, fuse};

use super::{
    filesystem::Mount,
    protocol::{self, Caller, Fields, Reader},
};
use crate::{
    fs::{
        CurrentRequester, FileSystemError,
        chardev::{DeviceError, UserFault, UserInput, UserOutput},
    },
    ipc::{Pipe, PipeEnd, PipeWaitCondition},
    namespace::PidNamespace,
    sync::{TaskEvent, WaitResult},
};

/// 内核声明支持、守护进程可选择启用的 INIT 能力（Linux `fuse_new_init` 的子集）。
const SUPPORTED: u64 = fuse::ASYNC_READ
    | fuse::BIG_WRITES
    | fuse::AUTO_INVAL_DATA
    | fuse::WRITEBACK_CACHE
    | fuse::DO_READDIRPLUS
    | fuse::READDIRPLUS_AUTO
    | fuse::PARALLEL_DIROPS
    | fuse::ABORT_ERROR
    | fuse::MAX_PAGES
    | fuse::INIT_EXT;
/// INIT 中提出的预读上限（Linux 缺省 bdi `ra_pages` 128 KiB，与页缓存一次填充上限一致）。
const MAX_READAHEAD: u32 = 128 * 1024;
/// Linux `fuse_max_pages_limit` 缺省值：单个请求最多 256 页（1 MiB）。
const MAX_PAGES_LIMIT: u32 = 256;
/// 未协商 FUSE_MAX_PAGES 时的单请求页数（Linux `FUSE_DEFAULT_MAX_PAGES_PER_REQ`）。
const DEFAULT_MAX_PAGES: u32 = 32;
/// 守护进程连续读取 FORGET 的配额；用尽后让普通请求先行（Linux `forget_batch`）。
const FORGET_BATCH: i32 = 16;
const PAGE_SIZE: u32 = crate::memory::PAGE_SIZE as u32;
const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;

/// INIT 回复协商出的连接参数。
#[derive(Debug, Clone, Copy)]
pub(super) struct Negotiated {
    /// 守护进程的协议次版本。
    pub(super) minor: u32,
    /// 双方都支持的 INIT flags。
    pub(super) flags: u64,
    /// 单个 WRITE 携带的最大数据量。
    pub(super) max_write: u32,
    /// 单个请求的最大页数。
    pub(super) max_pages: u32,
    /// 守护进程接受的预读字节数（不超过内核在 INIT 中提出的 [`MAX_READAHEAD`]）。
    pub(super) max_readahead: u32,
}

impl Negotiated {
    pub(super) const fn has(&self, flag: u64) -> bool {
        self.flags & flag != 0
    }

    /// WRITE 分块大小（Linux `fuse_perform_write`：没有 BIG_WRITES 时每次一页）。
    pub(super) fn write_chunk(&self) -> usize {
        if self.has(fuse::BIG_WRITES) {
            self.max_write.min(self.max_pages * PAGE_SIZE) as usize
        } else {
            PAGE_SIZE as usize
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self, FileSystemError> {
        let mut reader = Reader::new(bytes);
        let major = reader.u32()?;
        let minor = reader.u32()?;
        // 7.12 之前的守护进程使用更短的 entry/mknod 结构；本实现不提供这些兼容尺寸。
        if major != fuse::KERNEL_VERSION || minor < 12 {
            return Err(FileSystemError::InvalidOperation);
        }
        let mut field = |size: usize| -> u64 {
            if reader.remaining() < size {
                return 0;
            }
            match size {
                2 => reader.u16().map_or(0, u64::from),
                _ => reader.u32().map_or(0, u64::from),
            }
        };
        let max_readahead = field(4) as u32;
        let mut flags = field(4);
        let _max_background = field(2);
        let _congestion_threshold = field(2);
        let max_write = field(4) as u32;
        let _time_gran = field(4);
        let max_pages = field(2) as u32;
        let _map_alignment = field(2);
        let flags2 = field(4);
        if flags & fuse::INIT_EXT != 0 {
            flags |= flags2 << 32;
        }
        let flags = flags & SUPPORTED;
        Ok(Self {
            minor,
            flags,
            // Linux：协议次版本 >= 5 才有 max_write，且至少一页。
            max_write: max_write.max(PAGE_SIZE),
            max_readahead: max_readahead.min(MAX_READAHEAD),
            max_pages: if flags & fuse::MAX_PAGES != 0 {
                max_pages.clamp(1, MAX_PAGES_LIMIT)
            } else {
                DEFAULT_MAX_PAGES
            },
        })
    }
}

/// 回复负载的形状（Linux `fuse_args.out_args` 与 `out_argvar`）。
#[derive(Debug, Clone, Copy)]
pub(super) struct ReplyShape {
    size: usize,
    variable: bool,
}

impl ReplyShape {
    /// 只有回复头（UNLINK、RELEASE 等）。
    pub(super) const EMPTY: Self = Self::fixed(0);

    pub(super) const fn fixed(size: usize) -> Self {
        Self {
            size,
            variable: false,
        }
    }

    /// 至多 `size` 字节（READ、READDIR、READLINK、INIT）。
    pub(super) const fn up_to(size: usize) -> Self {
        Self {
            size,
            variable: true,
        }
    }
}

enum Phase {
    Queued,
    Sent { device: u64 },
    Done(Result<Vec<u8>, FileSystemError>),
    Taken,
}

/// 一个请求；消息在构造时定稿，守护进程读取时无锁复制。
pub(super) struct Request {
    unique: u64,
    opcode: u32,
    message: Vec<u8>,
    reply: ReplyShape,
    phase: Mutex<Phase>,
    // OWNER: 只有发起请求的线程等待；回复、中止与读取失败 signal，持锁时不调用 scheduler。
    done: TaskEvent,
    // OWNER: 调用者已被 signal 中断（Linux FR_INTERRUPTED）；请求在此之后才被读走时，读取者负责
    // 补发 INTERRUPT。缺失时请求在等待中途被读走就再也收不到中断。
    interrupted: AtomicBool,
}

impl Request {
    fn finish(&self, result: Result<Vec<u8>, FileSystemError>) {
        *self.phase.lock() = Phase::Done(result);
        self.done.signal();
    }

    fn take(&self) -> Option<Result<Vec<u8>, FileSystemError>> {
        let mut phase = self.phase.lock();
        if !matches!(*phase, Phase::Done(_)) {
            return None;
        }
        match core::mem::replace(&mut *phase, Phase::Taken) {
            Phase::Done(result) => Some(result),
            _ => unreachable!(),
        }
    }
}

/// 预先构造并占好队列槽位的后台请求（RELEASE）：Drop 路径入队不分配、不失败。
pub(super) struct Reserved {
    connection: Arc<Connection>,
    request: Option<Arc<Request>>,
}

impl Reserved {
    /// 在发出前改写参数区（OPEN 回复后才知道 RELEASE 的文件句柄）。
    pub(super) fn patch(&mut self, offset: usize, bytes: &[u8]) {
        let request = Arc::get_mut(self.request.as_mut().expect("reserved request present"))
            .expect("unsent reserved request is unshared");
        let start = fuse::IN_HEADER_SIZE + offset;
        request.message[start..start + bytes.len()].copy_from_slice(bytes);
    }

    /// 在发出前改写请求头的 nodeid（CREATE 回复后才知道新文件的 nodeid）。
    pub(super) fn set_nodeid(&mut self, nodeid: u64) {
        let request = Arc::get_mut(self.request.as_mut().expect("reserved request present"))
            .expect("unsent reserved request is unshared");
        request.message[16..24].copy_from_slice(&nodeid.to_ne_bytes());
    }

    /// 发出请求，不等待回复。
    pub(super) fn send(mut self) {
        let request = self.request.take().expect("reserved request sent twice");
        self.connection.release_slot(Some(request));
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        if self.request.is_some() {
            self.connection.release_slot(None);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Connected,
    /// 卸载或最后一个设备关闭（Linux `fuse_abort_conn`，`fc->aborted == false`）。
    Detached,
    /// `/sys/fs/fuse/connections/<dev>/abort` 主动中止。
    Aborted,
}

struct Queue {
    link: Link,
    initialized: bool,
    /// INIT 失败（Linux `conn_error`）：此后请求返回 `ECONNREFUSED`。
    failed: bool,
    /// 守护进程不支持中断（INTERRUPT 回复 `ENOSYS`）。
    no_interrupt: bool,
    pending: VecDeque<Arc<Request>>,
    /// `pending` 中为后台请求预留的空位数；不变量：`capacity - len >= reserved`。
    reserved: usize,
    interrupts: VecDeque<Arc<Request>>,
    forgets: VecDeque<(u64, u64)>,
    /// 为存活 inode 预留的 FORGET 空位；不变量与 `reserved` 相同。
    forget_slots: usize,
    forget_batch: i32,
    processing: Vec<Arc<Request>>,
}

/// 一个 FUSE 挂载与其守护进程之间的连接。
pub(super) struct Connection {
    queue: Mutex<Queue>,
    // OWNER: 守护进程 poll/read 的唤醒源；入队、中断、FORGET 与中止发布 readiness。
    readiness: (Arc<PipeEnd>, Arc<PipeEnd>),
    // OWNER: INIT 完成或连接中止的一次性 latch，从不 drain；INIT 前到达的请求在此等待
    // （Linux `blocked_waitq`）。缺失时请求会先于 INIT 发给守护进程，违反协议。
    initialized: (Arc<PipeEnd>, Arc<PipeEnd>),
    negotiated: Once<Negotiated>,
    // OWNER: 请求 unique 计数，步长 2；最低位留给 INTERRUPT 的回复 ID（Linux FUSE_INT_REQ_BIT）。
    next_unique: AtomicU64,
    // OWNER: 绑定到本连接的设备文件数；归零即中止（Linux 最后一个 `fuse_dev` release）。
    devices: AtomicUsize,
    // OWNER: 设备标识计数；回复只能写回读走请求的那个设备（Linux 每个 `fuse_dev` 自有处理队列）。
    next_device: AtomicU64,
    requester: CurrentRequester,
    pid_namespace: Arc<PidNamespace>,
    // OWNER: 守护进程通知（INVAL_INODE/INVAL_ENTRY/DELETE）找回挂载的弱引用；挂载持有连接，
    // 反向用 Weak 不形成环。卸载后升级失败，通知按 ENOENT 拒绝。
    mount: Once<Weak<Mount>>,
    /// 挂载选项 `max_read`。
    pub(super) max_read: u32,
}

impl Connection {
    /// 创建连接并排入 INIT（Linux `fuse_send_init`）。
    ///
    /// # Errors
    ///
    /// 分配失败返回 `OutOfMemory`。
    pub(super) fn new(
        requester: CurrentRequester,
        pid_namespace: Arc<PidNamespace>,
        max_read: u32,
    ) -> Result<Arc<Self>, FileSystemError> {
        let pair = || Pipe::notification_pair().map_err(|_| FileSystemError::OutOfMemory);
        let connection = Arc::try_new(Self {
            queue: Mutex::new(Queue {
                link: Link::Connected,
                initialized: false,
                failed: false,
                no_interrupt: false,
                pending: VecDeque::new(),
                reserved: 0,
                interrupts: VecDeque::new(),
                forgets: VecDeque::new(),
                forget_slots: 0,
                forget_batch: FORGET_BATCH,
                processing: Vec::new(),
            }),
            readiness: pair()?,
            initialized: pair()?,
            negotiated: Once::new(),
            next_unique: AtomicU64::new(0),
            devices: AtomicUsize::new(0),
            next_device: AtomicU64::new(1),
            requester,
            pid_namespace,
            mount: Once::new(),
            max_read,
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let body = Fields::<{ fuse::INIT_IN_SIZE }>::new()
            .u32(fuse::KERNEL_VERSION)
            .u32(fuse::KERNEL_MINOR_VERSION)
            .u32(MAX_READAHEAD)
            .u32(SUPPORTED as u32)
            .u32((SUPPORTED >> 32) as u32)
            .finish();
        let request = connection.request(
            fuse::INIT,
            0,
            &[&body],
            ReplyShape::up_to(fuse::INIT_OUT_SIZE),
        )?;
        connection.enqueue(request, 0)?;
        Ok(connection)
    }

    /// 记录本连接服务的挂载。
    pub(super) fn attach_mount(&self, mount: &Arc<Mount>) {
        self.mount.call_once(|| Arc::downgrade(mount));
    }

    /// 本连接服务的挂载；卸载后为 `None`。
    pub(super) fn mount(&self) -> Option<Arc<Mount>> {
        self.mount.get().and_then(Weak::upgrade)
    }

    /// 当前调用者是否可以访问挂载（Linux `fuse_allow_current_process`）：`allow_other` 之外只允许
    /// 挂载用户本人；内核线程（写回）总是允许。
    pub(super) fn current_allowed(&self, user_id: u32, group_id: u32) -> bool {
        (self.requester)().is_none_or(|requester| {
            requester.identity.uid() == user_id && requester.identity.gid() == group_id
        })
    }

    /// INIT 协商结果；INIT 尚未完成时为 `None`。
    pub(super) fn negotiated(&self) -> Option<&Negotiated> {
        self.negotiated.get()
    }

    fn caller(&self) -> Caller {
        (self.requester)().map_or_else(Caller::default, |requester| Caller {
            uid: requester.identity.uid(),
            gid: requester.identity.gid(),
            pid: requester.thread.number_in(&self.pid_namespace),
        })
    }

    fn request(
        &self,
        opcode: u32,
        nodeid: u64,
        arguments: &[&[u8]],
        reply: ReplyShape,
    ) -> Result<Arc<Request>, FileSystemError> {
        let unique = self.next_unique.fetch_add(2, Ordering::Relaxed) + 2;
        let message = protocol::request(opcode, unique, nodeid, self.caller(), arguments)?;
        Arc::try_new(Request {
            unique,
            opcode,
            message,
            reply,
            phase: Mutex::new(Phase::Queued),
            done: TaskEvent::new(),
            interrupted: AtomicBool::new(false),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn enqueue(&self, request: Arc<Request>, extra_reserve: usize) -> Result<(), FileSystemError> {
        let mut queue = self.queue.lock();
        if let Some(error) = queue.refusal() {
            return Err(error);
        }
        let needed = queue.reserved + extra_reserve + 1;
        queue
            .pending
            .try_reserve(needed)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        queue.pending.push_back(request);
        drop(queue);
        self.readiness.1.signal_readiness();
        Ok(())
    }

    /// 等待 INIT 完成（Linux `fuse_wait_initialized`）。
    fn wait_initialized(&self) -> Result<(), FileSystemError> {
        loop {
            {
                let queue = self.queue.lock();
                if let Some(error) = queue.refusal() {
                    return Err(error);
                }
                if queue.initialized {
                    return Ok(());
                }
            }
            match self
                .initialized
                .0
                .pipe()
                .wait(PipeWaitCondition::Readable, None)
            {
                WaitResult::Interrupted => return Err(FileSystemError::Errno(errno::EINTR as u16)),
                WaitResult::OutOfMemory => return Err(FileSystemError::OutOfMemory),
                WaitResult::Woken | WaitResult::TimedOut => {}
            }
        }
    }

    /// 发送请求并等待回复（Linux `fuse_simple_request`）。
    ///
    /// # Returns
    ///
    /// 回复负载（不含 `fuse_out_header`）。
    ///
    /// # Errors
    ///
    /// 守护进程回复的错误；连接断开返回 `ENOTCONN`/`ECONNABORTED`，INIT 失败返回 `ECONNREFUSED`，
    /// 尚未发出即被致命 signal 打断返回 `EINTR`。
    pub(super) fn call(
        &self,
        opcode: u32,
        nodeid: u64,
        arguments: &[&[u8]],
        reply: ReplyShape,
    ) -> Result<Vec<u8>, FileSystemError> {
        self.wait_initialized()?;
        let request = self.request(opcode, nodeid, arguments, reply)?;
        self.enqueue(request.clone(), 0)?;
        self.wait_answer(&request)
    }

    /// 1. 可中断等待；被 signal 打断时标记中断，已发出的请求补发 INTERRUPT；
    /// 2. 只响应 SIGKILL 的等待；仍在队列中的请求此时撤回并返回 `EINTR`；
    /// 3. 已发出的请求不可撤回，不可中断地等到回复或中止（Linux `request_wait_answer`）。
    fn wait_answer(&self, request: &Arc<Request>) -> Result<Vec<u8>, FileSystemError> {
        if !self.queue.lock().no_interrupt {
            loop {
                if let Some(result) = request.take() {
                    return result;
                }
                if request.done.wait_interruptible() == WaitResult::Interrupted {
                    break;
                }
            }
            request.interrupted.store(true, Ordering::Release);
            if matches!(*request.phase.lock(), Phase::Sent { .. }) {
                self.queue_interrupt(request);
            }
        }
        loop {
            if let Some(result) = request.take() {
                return result;
            }
            if request.done.wait_killable() == WaitResult::Interrupted {
                let mut queue = self.queue.lock();
                if matches!(*request.phase.lock(), Phase::Queued)
                    && let Some(index) = queue
                        .pending
                        .iter()
                        .position(|queued| Arc::ptr_eq(queued, request))
                {
                    queue.pending.remove(index);
                    return Err(FileSystemError::Errno(errno::EINTR as u16));
                }
                break;
            }
        }
        loop {
            if let Some(result) = request.take() {
                return result;
            }
            request.done.wait();
        }
    }

    fn queue_interrupt(&self, request: &Arc<Request>) {
        let mut queue = self.queue.lock();
        // 已完成或已在中断队列中的请求不再排队；分配失败时放弃中断，调用者继续等待回复。
        if queue.link != Link::Connected
            || queue.no_interrupt
            || !matches!(*request.phase.lock(), Phase::Sent { .. })
            || queue
                .interrupts
                .iter()
                .any(|queued| Arc::ptr_eq(queued, request))
            || queue.interrupts.try_reserve(1).is_err()
        {
            return;
        }
        queue.interrupts.push_back(request.clone());
        drop(queue);
        self.readiness.1.signal_readiness();
    }

    /// 构造并预留一个后台请求；之后在 Drop 等不能失败的路径发出（Linux RELEASE 预分配的 `fuse_file`）。
    ///
    /// # Errors
    ///
    /// 分配失败返回 `OutOfMemory`；连接已断开返回对应 errno。
    pub(super) fn reserve(
        self: &Arc<Self>,
        opcode: u32,
        nodeid: u64,
        arguments: &[&[u8]],
    ) -> Result<Reserved, FileSystemError> {
        let request = self.request(opcode, nodeid, arguments, ReplyShape::EMPTY)?;
        let mut queue = self.queue.lock();
        let needed = queue.reserved + 1;
        queue
            .pending
            .try_reserve(needed)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        queue.reserved = needed;
        drop(queue);
        Ok(Reserved {
            connection: self.clone(),
            request: Some(request),
        })
    }

    fn release_slot(&self, request: Option<Arc<Request>>) {
        let mut queue = self.queue.lock();
        queue.reserved -= 1;
        let Some(request) = request.filter(|_| queue.link == Link::Connected) else {
            return;
        };
        // 预留保证容量，push 不会分配。
        queue.pending.push_back(request);
        drop(queue);
        self.readiness.1.signal_readiness();
    }

    /// 为 `count` 次可能的查找预留 FORGET 空位；每个空位由 [`Connection::forget`] 恰好消费一次，
    /// 或由 [`Connection::unreserve_forgets`] 退还，inode 释放时因此无分配入队。
    pub(super) fn reserve_forgets(&self, count: usize) -> Result<(), FileSystemError> {
        let mut queue = self.queue.lock();
        let needed = queue.forget_slots + count;
        queue
            .forgets
            .try_reserve(needed)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        queue.forget_slots = needed;
        Ok(())
    }

    /// 退还未用到的 FORGET 预留。
    pub(super) fn unreserve_forgets(&self, count: usize) {
        self.queue.lock().forget_slots -= count;
    }

    /// 消费一个 FORGET 预留：`lookups` 非零时告知守护进程释放这么多查找引用。
    pub(super) fn forget(&self, nodeid: u64, lookups: u64) {
        let mut queue = self.queue.lock();
        queue.forget_slots -= 1;
        if lookups == 0 || queue.link != Link::Connected {
            return;
        }
        queue.forgets.push_back((nodeid, lookups));
        drop(queue);
        self.readiness.1.signal_readiness();
    }

    /// 绑定一个设备文件，返回其设备标识。
    pub(super) fn attach_device(&self) -> u64 {
        self.devices.fetch_add(1, Ordering::Relaxed);
        self.next_device.fetch_add(1, Ordering::Relaxed)
    }

    /// 设备文件最后一次释放：结束它读走、尚未回复的请求；最后一个设备关闭即中止连接。
    pub(super) fn detach_device(&self, device: u64) {
        let ended = {
            let mut queue = self.queue.lock();
            let mut ended = Vec::new();
            if ended.try_reserve(queue.processing.len()).is_ok() {
                queue.processing.retain(|request| {
                    let owned =
                        matches!(*request.phase.lock(), Phase::Sent { device: owner } if owner == device);
                    if owned {
                        ended.push(request.clone());
                    }
                    !owned
                });
            }
            ended
        };
        for request in ended {
            request.finish(Err(FileSystemError::Errno(errno::ECONNABORTED as u16)));
        }
        if self.devices.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.abort(false);
        }
    }

    /// 中止连接：此后的请求立即失败，未完成请求以 `ECONNABORTED`（协商了 ABORT_ERROR 或主动中止）
    /// 或 `ENOTCONN` 结束，守护进程 read 返回 `ENODEV`/`ECONNABORTED`。
    pub(super) fn abort(&self, explicit: bool) {
        let (pending, processing, error) = {
            let mut queue = self.queue.lock();
            if queue.link != Link::Connected {
                return;
            }
            queue.link = if explicit {
                Link::Aborted
            } else {
                Link::Detached
            };
            let abort_error = explicit
                || self
                    .negotiated
                    .get()
                    .is_some_and(|negotiated| negotiated.has(fuse::ABORT_ERROR));
            queue.interrupts.clear();
            queue.forgets.clear();
            (
                core::mem::take(&mut queue.pending),
                core::mem::take(&mut queue.processing),
                if abort_error {
                    errno::ECONNABORTED
                } else {
                    errno::ENOTCONN
                },
            )
        };
        for request in pending.into_iter().chain(processing) {
            request.finish(Err(FileSystemError::Errno(error as u16)));
        }
        self.readiness.1.signal_readiness();
        self.initialized.1.signal_readiness();
    }

    fn finish_init(&self, result: Result<&[u8], FileSystemError>) {
        let negotiated = result.and_then(Negotiated::parse);
        if let Ok(negotiated) = negotiated {
            self.negotiated.call_once(|| negotiated);
        }
        {
            let mut queue = self.queue.lock();
            queue.initialized = true;
            queue.failed = negotiated.is_err();
        }
        self.initialized.1.signal_readiness();
    }

    /// 守护进程 read（Linux `fuse_dev_do_read`）：中断优先，其次按配额交替 FORGET 与普通请求。
    pub(super) fn read(
        &self,
        device: u64,
        output: &mut dyn UserOutput,
        nonblocking: bool,
        wait: impl Fn() -> Result<(), DeviceError>,
    ) -> Result<(), DeviceError> {
        let max_write = self
            .negotiated
            .get()
            .map_or(0, |negotiated| negotiated.max_write as usize);
        if output.remaining() < fuse::MIN_READ_BUFFER.max(fuse::IN_HEADER_SIZE + 40 + max_write) {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        loop {
            let mut queue = self.queue.lock();
            match queue.link {
                Link::Connected => {}
                Link::Detached => return Err(DeviceError::Errno(errno::ENODEV)),
                Link::Aborted => return Err(DeviceError::Errno(errno::ECONNABORTED)),
            }
            if let Some(request) = queue.interrupts.pop_front() {
                drop(queue);
                return self.write_interrupt(&request, output);
            }
            if !queue.forgets.is_empty() {
                let take_forget = queue.pending.is_empty() || {
                    queue.forget_batch -= 1;
                    queue.forget_batch >= 0
                };
                if take_forget {
                    return self.write_forgets(queue, output);
                }
                if queue.forget_batch <= -8 {
                    queue.forget_batch = FORGET_BATCH;
                }
            }
            let Some(request) = queue.pending.pop_front() else {
                drop(queue);
                if nonblocking {
                    return Err(DeviceError::WouldBlock);
                }
                wait()?;
                continue;
            };
            if request.message.len() > output.remaining() {
                drop(queue);
                request.finish(Err(FileSystemError::IoError));
                continue;
            }
            if queue.processing.try_reserve(1).is_err() {
                // 刚弹出一个元素，push_front 不会分配。
                queue.pending.push_front(request);
                return Err(DeviceError::Errno(errno::ENOMEM));
            }
            *request.phase.lock() = Phase::Sent { device };
            queue.processing.push(request.clone());
            drop(queue);
            if output.write(&request.message).is_err() {
                self.take_processing(request.unique, device);
                request.finish(Err(FileSystemError::IoError));
                return Err(DeviceError::Errno(errno::EFAULT));
            }
            if request.interrupted.load(Ordering::Acquire) {
                self.queue_interrupt(&request);
            }
            return Ok(());
        }
    }

    fn write_interrupt(
        &self,
        request: &Request,
        output: &mut dyn UserOutput,
    ) -> Result<(), DeviceError> {
        const LENGTH: usize = fuse::IN_HEADER_SIZE + 8;
        let header = protocol::header(
            LENGTH as u32,
            fuse::INTERRUPT,
            request.unique | 1,
            0,
            Caller::default(),
        );
        output.write(&header).map_err(fault)?;
        output.write(&request.unique.to_ne_bytes()).map_err(fault)
    }

    fn write_forgets(
        &self,
        mut queue: spin::MutexGuard<'_, Queue>,
        output: &mut dyn UserOutput,
    ) -> Result<(), DeviceError> {
        let unique = self.next_unique.fetch_add(2, Ordering::Relaxed) + 2;
        let batch = self
            .negotiated
            .get()
            .is_some_and(|negotiated| negotiated.minor >= 16)
            && queue.forgets.len() > 1;
        if !batch {
            let (nodeid, lookups) = queue.forgets.pop_front().expect("forget pending");
            drop(queue);
            let header = protocol::header(
                (fuse::IN_HEADER_SIZE + 8) as u32,
                fuse::FORGET,
                unique,
                nodeid,
                Caller::default(),
            );
            output.write(&header).map_err(fault)?;
            return output.write(&lookups.to_ne_bytes()).map_err(fault);
        }
        let room = (output.remaining() - fuse::IN_HEADER_SIZE - 8) / 16;
        let count = queue.forgets.len().min(room);
        let mut entries = [(0u64, 0u64); 32];
        let count = count.min(entries.len());
        for entry in &mut entries[..count] {
            *entry = queue.forgets.pop_front().expect("counted forget");
        }
        drop(queue);
        let header = protocol::header(
            (fuse::IN_HEADER_SIZE + 8 + count * 16) as u32,
            fuse::BATCH_FORGET,
            unique,
            0,
            Caller::default(),
        );
        output.write(&header).map_err(fault)?;
        output
            .write(&Fields::<8>::new().u32(count as u32).finish())
            .map_err(fault)?;
        for (nodeid, lookups) in &entries[..count] {
            output
                .write(&Fields::<16>::new().u64(*nodeid).u64(*lookups).finish())
                .map_err(fault)?;
        }
        Ok(())
    }

    fn take_processing(&self, unique: u64, device: u64) -> Option<Arc<Request>> {
        let mut queue = self.queue.lock();
        let index = queue.processing.iter().position(|request| {
            request.unique == unique
                && matches!(*request.phase.lock(), Phase::Sent { device: owner } if owner == device)
        })?;
        let request = queue.processing.swap_remove(index);
        if request.interrupted.load(Ordering::Acquire)
            && let Some(index) = queue
                .interrupts
                .iter()
                .position(|queued| Arc::ptr_eq(queued, &request))
        {
            queue.interrupts.remove(index);
        }
        Some(request)
    }

    /// 守护进程 write（Linux `fuse_dev_do_write`）：回复、中断回复或通知。
    ///
    /// `notify` 处理 unique 为零的通知消息（错误字段是通知码，负载已复制）。
    pub(super) fn write(
        &self,
        device: u64,
        input: &mut dyn UserInput,
        notify: impl FnOnce(i32, &[u8]) -> Result<(), FileSystemError>,
    ) -> Result<(), DeviceError> {
        let length = input.remaining();
        if length < fuse::OUT_HEADER_SIZE {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let mut header = [0u8; fuse::OUT_HEADER_SIZE];
        input.copy(&mut header).map_err(fault)?;
        let mut reader = Reader::new(&header);
        let declared = reader.u32().unwrap() as usize;
        let error = reader.u32().unwrap() as i32;
        let unique = reader.u64().unwrap();
        if declared != length {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let payload_length = length - fuse::OUT_HEADER_SIZE;
        if unique == 0 {
            let payload = copy_payload(input, payload_length)?;
            notify(error, &payload[fuse::OUT_HEADER_SIZE..]).map_err(device_error)?;
            input.consume(length);
            return Ok(());
        }
        if !(-511..=0).contains(&error) {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        if unique & 1 != 0 {
            let request = {
                let queue = self.queue.lock();
                queue
                    .processing
                    .iter()
                    .find(|request| {
                        request.unique == unique & !1
                            && matches!(*request.phase.lock(), Phase::Sent { device: owner } if owner == device)
                    })
                    .cloned()
            }
            .ok_or(DeviceError::Errno(errno::ENOENT))?;
            if payload_length != 0 {
                return Err(DeviceError::Errno(errno::EINVAL));
            }
            match -(error as isize) {
                errno::ENOSYS => self.queue.lock().no_interrupt = true,
                errno::EAGAIN => self.queue_interrupt(&request),
                _ => {}
            }
            input.consume(length);
            return Ok(());
        }
        let request = self
            .take_processing(unique, device)
            .ok_or(DeviceError::Errno(errno::ENOENT))?;
        let result = if error != 0 {
            if payload_length != 0 {
                Err(errno::EINVAL)
            } else {
                Ok(Err(protocol::reply_error((-error) as u16)))
            }
        } else if payload_length > request.reply.size
            || payload_length < request.reply.size && !request.reply.variable
        {
            Err(errno::EINVAL)
        } else {
            match copy_payload(input, payload_length) {
                Ok(mut payload) => {
                    payload.drain(..fuse::OUT_HEADER_SIZE);
                    Ok(Ok(payload))
                }
                Err(DeviceError::Errno(errno::ENOMEM)) => Ok(Err(FileSystemError::OutOfMemory)),
                Err(error) => {
                    request.finish(Err(FileSystemError::IoError));
                    return Err(error);
                }
            }
        };
        match result {
            Ok(reply) => {
                if request.opcode == fuse::INIT {
                    self.finish_init(reply.as_deref().map_err(|error| *error));
                }
                request.finish(reply);
                input.consume(length);
                Ok(())
            }
            Err(code) => {
                if request.opcode == fuse::INIT {
                    self.finish_init(Err(FileSystemError::IoError));
                }
                request.finish(Err(FileSystemError::IoError));
                Err(DeviceError::Errno(code))
            }
        }
    }

    /// 守护进程可读（有请求）、总可写；连接断开报告 POLLERR（Linux `fuse_dev_poll`）。
    pub(super) fn poll(&self, events: i16) -> i16 {
        let queue = self.queue.lock();
        let mut ready = POLLOUT | POLLWRNORM;
        if queue.link != Link::Connected {
            ready |= POLLERR;
        } else if !queue.pending.is_empty()
            || !queue.interrupts.is_empty()
            || !queue.forgets.is_empty()
        {
            ready |= POLLIN | POLLRDNORM;
        }
        ready & (events | POLLERR)
    }

    pub(super) fn readiness_pipe(&self) -> Arc<Pipe> {
        self.readiness.0.pipe()
    }

    pub(super) fn drain_readiness(&self) {
        self.readiness.0.drain_readiness();
    }
}

impl Queue {
    fn refusal(&self) -> Option<FileSystemError> {
        match self.link {
            Link::Connected if self.failed => {
                Some(FileSystemError::Errno(errno::ECONNREFUSED as u16))
            }
            Link::Connected => None,
            Link::Detached => Some(FileSystemError::Errno(errno::ENOTCONN as u16)),
            Link::Aborted => Some(FileSystemError::Errno(errno::ECONNABORTED as u16)),
        }
    }
}

/// 复制回复：前 16 字节为已读的头部占位，后接负载；调用者确认后才 consume。
fn copy_payload(input: &dyn UserInput, payload: usize) -> Result<Vec<u8>, DeviceError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(fuse::OUT_HEADER_SIZE + payload)
        .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
    bytes.resize(fuse::OUT_HEADER_SIZE + payload, 0);
    input.copy(&mut bytes).map_err(fault)?;
    Ok(bytes)
}

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

fn device_error(error: FileSystemError) -> DeviceError {
    DeviceError::Errno(match error {
        FileSystemError::OutOfMemory => errno::ENOMEM,
        FileSystemError::NotFound => errno::ENOENT,
        FileSystemError::NotDirectory => errno::ENOTDIR,
        FileSystemError::Busy => errno::EBUSY,
        FileSystemError::Errno(value) => value as isize,
        _ => errno::EINVAL,
    })
}
