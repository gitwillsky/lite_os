//! 标准 FUSE SQE128 传输。登记、请求路由与处理队列同属 Connection.queue；用户复制与通知在锁外。
use alloc::boxed::Box;

use super::*;
use crate::{
    cpu::CpuId,
    fs::{
        CompletionNotify,
        chardev::{PendingCommand, UringCommand, UserMemory},
    },
};

/// INIT 选择 Registering，全部 CPU 就绪后 Active；登记失败只在切换前退回 Legacy。
enum Stage {
    Legacy,
    Registering,
    Active,
}

/// 唯一连接锁内的传输阶段与登记表；不另外持有请求引用或 CPU topology 副本。
pub(super) struct Transport {
    stage: Stage,
    entries: Vec<Entry>,
}

impl Transport {
    pub(super) fn new() -> Self {
        Self {
            stage: Stage::Legacy,
            entries: Vec::new(),
        }
    }

    pub(super) fn negotiate(&mut self, enabled: bool) {
        if enabled {
            self.stage = Stage::Registering;
        }
    }

    pub(super) fn registering(&self) -> bool {
        matches!(self.stage, Stage::Registering)
    }
}

#[derive(Clone, Copy)]
struct Buffers {
    header: usize,
    payload: usize,
}

struct Entry {
    id: u64,
    cpu: CpuId,
    buffers: Buffers,
    state: EntryState,
}

enum EntryState {
    /// Weak 通知防止 Connection → Waiter → Ring → pending OFD → Connection 引用环。
    Available(Option<Weak<dyn CompletionNotify>>),
    /// 已交付的 request unique；唯一请求引用仍在 queue.processing，不复制协议状态。
    Userspace(u64),
    /// COMMIT 已认领登记项，复制期间拒绝同一 commit 的并发重用。
    Committing,
}

/// 每个未完成 SQE 一个令牌。Drop 撤销可用登记项，已交付请求由下一次 COMMIT 或 abort 结束。
struct Command {
    connection: Arc<Connection>,
    entry: Option<u64>,
}

impl Queue {
    pub(super) fn route(&self, request: &Request) {
        let cpu = matches!(self.uring.stage, Stage::Active).then(crate::cpu::current_id);
        *request.phase.lock() = Phase::Queued { cpu };
    }

    /// Linux ring 后台预算允许每个空闲 CPU 队列至少发出一个请求，防止一个队列耗尽全局预算。
    pub(super) fn idle_ring_queue(&self, request: &Request) -> bool {
        let cpu = match *request.phase.lock() {
            Phase::Queued { cpu: Some(cpu) } => cpu,
            _ => return false,
        };
        !self.pending.iter().chain(&self.processing).any(|active| {
            active.background
                && match *active.phase.lock() {
                    Phase::Queued { cpu: Some(owner) }
                    | Phase::RingCopying { cpu: owner, .. }
                    | Phase::Ring { cpu: owner, .. } => owner == cpu,
                    _ => false,
                }
        })
    }
}

impl Connection {
    /// 在普通 readiness 之外唤醒已登记的 ring 命令；不在 queue 锁内升级 Weak/回调。
    /// 按单调 entry ID 逐个取快照，不为 completion/RELEASE/abort 分配临时集合。
    pub(super) fn wake_requests(&self) {
        self.readiness.1.signal_readiness();
        // 本轮只访问调用时已存在的登记项，持续 REGISTER 不能无限延长完成/abort 路径。
        let last = self
            .queue
            .lock()
            .uring
            .entries
            .last()
            .map_or(0, |entry| entry.id);
        let mut cursor = 0;
        loop {
            let next = {
                let queue = self.queue.lock();
                let entries = &queue.uring.entries;
                let start = entries.partition_point(|entry| entry.id <= cursor);
                entries[start..].iter().take_while(|entry| entry.id <= last)
                    .filter(|entry| queue.link != Link::Connected
                        || matches!(queue.uring.stage, Stage::Legacy)
                        || queue.pending.iter().any(|request| matches!(*request.phase.lock(), Phase::Queued { cpu: Some(cpu) } if cpu == entry.cpu)))
                    .find_map(|entry| match &entry.state {
                        EntryState::Available(Some(notify)) => Some((entry.id, notify.clone())),
                        _ => None,
                    })
            };
            let Some((id, notify)) = next else { break };
            cursor = id;
            if let Some(notify) = notify.upgrade() {
                notify.notify();
            }
        }
    }

    /// 设备文件的唯一 uring_cmd 入口；FUSE 参数与队列不泄漏到 syscall/io_uring。
    ///
    /// # Parameters
    /// - `command`: SQE128 的标准 REGISTER 或 COMMIT_AND_FETCH。
    /// - `user`: 本次提交线程的地址空间；只通过该 seam 复制登记 iovec 与回复。
    ///
    /// # Returns
    /// 可通过 step 获取下一条请求的命令令牌。
    ///
    /// # Errors
    /// 非 SQE128/非法队列/短缓冲 EINVAL，未 INIT EAGAIN，未协商 EOPNOTSUPP，连接关闭 ENOTCONN，
    /// 主动 abort ECONNABORTED，未知 commit ENOENT，分配 ENOMEM，用户复制 EFAULT。
    pub(in crate::fs::fuse) fn uring_command(
        self: &Arc<Self>,
        command: &UringCommand,
        user: &dyn UserMemory,
    ) -> Result<Box<dyn PendingCommand>, DeviceError> {
        if !command.extended {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        self.command_connected()?;
        let negotiated = self.negotiated.get().ok_or(DeviceError::WouldBlock)?;
        if !negotiated.has(fuse::OVER_IO_URING) {
            return Err(DeviceError::Errno(errno::EOPNOTSUPP));
        }
        if !matches!(
            command.operation,
            fuse::IO_URING_CMD_REGISTER | fuse::IO_URING_CMD_COMMIT_AND_FETCH
        ) {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let result = (|| {
            let cpu = crate::cpu::id_at(
                u16::from_ne_bytes(command.data[16..18].try_into().unwrap()) as usize,
            )
            .ok_or(DeviceError::Errno(errno::EINVAL))?;
            // 1. 先分配可撤销令牌，再发布登记/认领 commit；OOM 不能留下没有等待者的登记项。
            let mut pending = Box::try_new(Command {
                connection: self.clone(),
                entry: None,
            })
            .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
            match command.operation {
                fuse::IO_URING_CMD_REGISTER => {
                    let buffers = register_buffers(command, user, negotiated)?;
                    let mut queue = self.queue.lock();
                    connected(&queue)?;
                    if matches!(queue.uring.stage, Stage::Legacy) {
                        return Err(DeviceError::Errno(errno::EOPNOTSUPP));
                    }
                    let id = crate::id::next_runtime_object_id();
                    queue
                        .uring
                        .entries
                        .try_reserve(1)
                        .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
                    // 每个 entry 最多一个在途请求；在 REGISTER 预留 processing 槽，交付不能再因 OOM
                    // 撤掉唯一 CPU 登记项，让已经排队的 FUSE caller 永久失去接收者。
                    let processing = queue.uring.entries.len() + 1;
                    queue
                        .processing
                        .try_reserve(processing)
                        .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
                    queue.uring.entries.push(Entry {
                        id,
                        cpu,
                        buffers,
                        state: EntryState::Available(None),
                    });
                    pending.entry = Some(id);
                    if queue.uring.registering()
                        && crate::cpu::possible().iter().all(|cpu| {
                            queue.uring.entries.iter().any(|entry| {
                                entry.cpu == cpu && matches!(entry.state, EntryState::Available(_))
                            })
                        })
                    {
                        queue.uring.stage = Stage::Active;
                        drop(queue);
                        self.initialized.1.signal_readiness();
                    }
                }
                fuse::IO_URING_CMD_COMMIT_AND_FETCH => {
                    let unique = u64::from_ne_bytes(command.data[8..16].try_into().unwrap());
                    // 2. 在同一锁内验证 qid/commit 与认领状态；复制期间 processing 保活请求并接受 abort。
                    let (request, buffers, id) = {
                        let mut queue = self.queue.lock();
                        connected(&queue)?;
                        let index = queue.uring.entries.iter().position(|entry| {
                        entry.cpu == cpu && matches!(entry.state, EntryState::Userspace(owner) if owner == unique)
                    }).ok_or(DeviceError::Errno(errno::ENOENT))?;
                        let entry = &mut queue.uring.entries[index];
                        let id = entry.id;
                        let buffers = entry.buffers;
                        entry.state = EntryState::Committing;
                        let request = queue.processing.iter().find(|request| {
                        matches!(*request.phase.lock(), Phase::Ring { entry, .. } if entry == id)
                    }).expect("userspace ring entry owns a processing request").clone();
                        (request, buffers, id)
                    };
                    let reply = read_reply(user, buffers, negotiated, &request);
                    self.end_ring_request(&request, reply);
                    let mut queue = self.queue.lock();
                    if let Some(entry) = queue.uring.entries.iter_mut().find(|entry| entry.id == id)
                    {
                        entry.state = EntryState::Available(None);
                    }
                    pending.entry = Some(id);
                }
                _ => return Err(DeviceError::Errno(errno::EINVAL)),
            }
            Ok(pending as Box<dyn PendingCommand>)
        })();
        if command.operation == fuse::IO_URING_CMD_REGISTER && result.is_err() {
            let fallback = {
                let mut queue = self.queue.lock();
                if queue.uring.registering() {
                    queue.uring.stage = Stage::Legacy;
                    true
                } else {
                    false
                }
            };
            if fallback {
                self.initialized.1.signal_readiness();
                self.wake_requests();
            }
        }
        result
    }

    fn command_connected(&self) -> Result<(), DeviceError> {
        let queue = self.queue.lock();
        connected(&queue)?;
        if !queue.initialized {
            return Err(DeviceError::WouldBlock);
        }
        Ok(())
    }

    fn end_ring_request(&self, request: &Request, result: Result<Vec<u8>, FileSystemError>) {
        let removed = {
            let mut queue = self.queue.lock();
            if let Some(index) = queue
                .interrupts
                .iter()
                .position(|queued| queued.unique == request.unique)
            {
                queue.interrupts.remove(index);
            }
            queue
                .processing
                .iter()
                .position(|active| active.unique == request.unique)
                .map(|index| queue.processing.swap_remove(index))
        };
        self.finish(request, result);
        drop(removed);
    }
}

impl PendingCommand for Command {
    fn ready(&self, notify: &Arc<dyn CompletionNotify>) -> bool {
        let mut queue = self.connection.queue.lock();
        if connected(&queue).is_err() || matches!(queue.uring.stage, Stage::Legacy) {
            return true;
        }
        let entry = queue
            .uring
            .entries
            .iter_mut()
            .find(|entry| Some(entry.id) == self.entry)
            .expect("pending command owns an entry");
        entry.state = EntryState::Available(Some(Arc::downgrade(notify)));
        let cpu = entry.cpu;
        queue.pending.iter().any(|request| {
            matches!(*request.phase.lock(), Phase::Queued { cpu: Some(owner) } if owner == cpu)
        })
    }

    fn step(
        &mut self,
        user: &dyn UserMemory,
        notify: &Arc<dyn CompletionNotify>,
    ) -> Option<Result<isize, DeviceError>> {
        let id = self.entry.expect("pending command has an entry");
        loop {
            // 1. 登记 notify 与检查 pending 使用同一 queue 锁；生产者在解锁后通知，不丢入队唤醒。
            let (request, buffers) = {
                let mut queue = self.connection.queue.lock();
                if queue.link != Link::Connected {
                    return Some(Err(DeviceError::Errno(errno::ENOTCONN)));
                }
                if matches!(queue.uring.stage, Stage::Legacy) {
                    return Some(Err(DeviceError::Errno(errno::ENOTCONN)));
                }
                let entry = queue
                    .uring
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .expect("live command retains registered entry");
                entry.state = EntryState::Available(Some(Arc::downgrade(notify)));
                let cpu = entry.cpu;
                let buffers = entry.buffers;
                let index = queue.pending.iter().position(|request| {
                    matches!(*request.phase.lock(), Phase::Queued { cpu: Some(owner) } if owner == cpu)
                })?;
                let request = queue.pending.remove(index).expect("ring pending index");
                *request.phase.lock() = Phase::RingCopying { cpu, entry: id };
                queue.processing.push(request.clone());
                queue
                    .uring
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .unwrap()
                    .state = EntryState::Committing;
                (request, buffers)
            };
            // 2. 用户页复制不持 queue/phase 锁；坏缓冲只结束这条 FUSE 请求，登记项继续获取下一条。
            if let Err(error) = write_request(
                user,
                buffers,
                &request,
                self.connection
                    .negotiated
                    .get()
                    .expect("ring requires INIT"),
            ) {
                self.connection
                    .end_ring_request(&request, Err(FileSystemError::Errno(error.errno() as u16)));
                continue;
            }
            let mut queue = self.connection.queue.lock();
            if queue.link != Link::Connected {
                return Some(Err(DeviceError::Errno(errno::ENOTCONN)));
            }
            let cpu = match *request.phase.lock() {
                Phase::RingCopying { cpu, entry } if entry == id => cpu,
                _ => unreachable!("connected copy retains its processing request"),
            };
            *request.phase.lock() = Phase::Ring { cpu, entry: id };
            queue
                .uring
                .entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .unwrap()
                .state = EntryState::Userspace(request.unique);
            drop(queue);
            if request.interrupted.load(Ordering::Acquire) {
                self.connection.queue_interrupt(&request);
            }
            // 3. CQE 成功后登记项继续存在，下一 SQE 以 commit_id 找回；Drop 不撤销用户仍持有的请求。
            self.entry = None;
            return Some(Ok(0));
        }
    }

    fn canceled_result(&self) -> isize {
        -errno::ENOTCONN
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        let Some(id) = self.entry else { return };
        let removed = {
            let mut queue = self.connection.queue.lock();
            queue
                .uring
                .entries
                .iter()
                .position(|entry| entry.id == id)
                .map(|index| queue.uring.entries.remove(index))
        };
        drop(removed);
    }
}

fn connected(queue: &Queue) -> Result<(), DeviceError> {
    match queue.link {
        Link::Connected => Ok(()),
        Link::Detached => Err(DeviceError::Errno(errno::ENOTCONN)),
        Link::Aborted => Err(DeviceError::Errno(errno::ECONNABORTED)),
    }
}

fn register_buffers(
    command: &UringCommand,
    user: &dyn UserMemory,
    negotiated: &Negotiated,
) -> Result<Buffers, DeviceError> {
    if command.length != 2 {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    let mut raw = [0u8; 32];
    user.read(command.address, &mut raw).map_err(fault)?;
    let word = |offset| u64::from_ne_bytes(raw[offset..offset + 8].try_into().unwrap()) as usize;
    let header = word(0);
    let payload = word(16);
    if word(8) < fuse::URING_HEADER_SIZE || word(24) < max_payload(negotiated) {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    if header.checked_add(word(8)).is_none() || payload.checked_add(word(24)).is_none() {
        return Err(DeviceError::Errno(errno::EFAULT));
    }
    Ok(Buffers { header, payload })
}

fn max_payload(negotiated: &Negotiated) -> usize {
    fuse::MIN_READ_BUFFER
        .max(negotiated.max_write as usize)
        .max(negotiated.max_pages as usize * PAGE_SIZE as usize)
}

fn write_request(
    user: &dyn UserMemory,
    buffers: Buffers,
    request: &Request,
    negotiated: &Negotiated,
) -> Result<(), DeviceError> {
    let body = &request.message[fuse::IN_HEADER_SIZE..];
    let mut header = [0u8; fuse::URING_HEADER_SIZE];
    header[..fuse::IN_HEADER_SIZE].copy_from_slice(&request.message[..fuse::IN_HEADER_SIZE]);
    header[fuse::URING_IN_OUT_HEADER_SIZE..][..request.operation_header]
        .copy_from_slice(&body[..request.operation_header]);
    let metadata = fuse::URING_IN_OUT_HEADER_SIZE + fuse::URING_OP_IN_OUT_SIZE;
    header[metadata + 8..metadata + 16].copy_from_slice(&request.unique.to_ne_bytes());
    let payload = &body[request.operation_header..];
    if payload.len() > max_payload(negotiated) {
        return Err(DeviceError::Errno(errno::EIO));
    }
    header[metadata + 16..metadata + 20].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
    if !payload.is_empty() {
        user.write(buffers.payload, payload).map_err(fault)?;
    }
    user.write(buffers.header, &header).map_err(fault)
}

fn read_reply(
    user: &dyn UserMemory,
    buffers: Buffers,
    negotiated: &Negotiated,
    request: &Request,
) -> Result<Vec<u8>, FileSystemError> {
    let mut header = [0u8; fuse::URING_HEADER_SIZE];
    user.read(buffers.header, &mut header)
        .map_err(|_| FileSystemError::Errno(errno::EFAULT as u16))?;
    let error = i32::from_ne_bytes(header[4..8].try_into().unwrap());
    let unique = u64::from_ne_bytes(header[8..16].try_into().unwrap());
    if unique == 0 || !(-511..=0).contains(&error) {
        return Err(FileSystemError::Errno(errno::EINVAL as u16));
    }
    if error != 0 {
        return Err(protocol::reply_error((-error) as u16));
    }
    if unique & !1 != request.unique {
        return Err(FileSystemError::Errno(errno::ENOENT as u16));
    }
    let offset = fuse::URING_IN_OUT_HEADER_SIZE + fuse::URING_OP_IN_OUT_SIZE + 16;
    let size = u32::from_ne_bytes(header[offset..offset + 4].try_into().unwrap()) as usize;
    if size > request.reply.size
        || size < request.reply.size && !request.reply.variable
        || size > max_payload(negotiated)
    {
        return Err(FileSystemError::Errno(errno::EINVAL as u16));
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(size)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    payload.resize(size, 0);
    if !payload.is_empty() {
        user.read(buffers.payload, &mut payload)
            .map_err(|_| FileSystemError::Errno(errno::EFAULT as u16))?;
    }
    Ok(payload)
}

/// Linux 为没有定长首参数的操作保留空 op_in；变长名称即便是本地首切片也只能进入 payload。
pub(super) fn operation_header(opcode: u32, arguments: &[&[u8]]) -> usize {
    match opcode {
        fuse::LOOKUP
        | fuse::SYMLINK
        | fuse::UNLINK
        | fuse::RMDIR
        | fuse::REMOVEXATTR
        | fuse::READLINK
        | fuse::STATFS
        | fuse::DESTROY => 0,
        _ => arguments.first().map_or(0, |argument| argument.len()),
    }
}
