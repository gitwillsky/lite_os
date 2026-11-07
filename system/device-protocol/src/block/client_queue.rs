//! 多调用者共享的块提交/完成环。一个当前调用者推进完成分派，其他调用者等本地 epoch；无常驻转发线程。
use super::*;
use core::sync::atomic::{AtomicU32, Ordering};
use native_runtime::native_abi::{DEADLINE_INFINITE, signals};
use native_runtime::{Event, Port, futex};
use queue::{Queue, Request};

struct Pending {
    ticket: u64,
    status: Option<Status>,
}
struct State {
    pending: Vec<Pending>,
    free: Vec<usize>,
    next: u64,
    // FLAG: 仅当前调用者推进 CQ/port；其他调用者等待 epoch，避免增加常驻分派线程。
    leader: bool,
    // FLAG: progress 的一次性异步登记尚未消费；否则会重复武装并无限积压通知。
    armed: bool,
    // FLAG: 实际 server close/进程退出已确认，此后不再发布请求。
    disconnected: bool,
    // FLAG: 协议异常只请求停止；已发表的请求仍等待完成或 confirmed close，不能借此提前归还 DMA。
    aborting: bool,
}
pub(super) struct ClientQueue {
    ring: Queue,
    ready: Event,
    progress: Event,
    port: Port,
    // OWNER: 本端唯一 SQ producer/CQ consumer、请求身份/credits 与完成分派权；不跨等待或设备 I/O。
    state: Mutex<State>,
    // OWNER: 完成分派/credits 归还后的本地唤醒序号；先快照再复查状态，避免交接时丢唤醒。
    changed: AtomicU32,
}
impl ClientQueue {
    pub(super) fn connect(channel: &Channel) -> Result<Self, Status> {
        let mut reply = call(channel, &Writer::request(QUEUE), Vec::new())?;
        let capacity = reply.reader().usize()?;
        let ring = Queue::open(
            Vmo::from_handle(take_handle(&mut reply.handles, 0)?),
            capacity,
        )?;
        let ready = Event::from_handle(take_handle(&mut reply.handles, 1)?);
        let progress = Event::from_handle(take_handle(&mut reply.handles, 2)?);
        // 连接时验证通知 rights；以后私有 RAII 句柄保持不变，signal 不分配且不能失败。
        ready.signal(0, 0)?;
        progress.signal(0, 0)?;
        let port = Port::create()?;
        channel.wait_async(&port, 0, signals::PEER_CLOSED)?;
        progress.wait_async(&port, 1, signals::SIGNALED)?;
        let mut pending = Vec::new();
        let mut free = Vec::new();
        pending
            .try_reserve_exact(capacity)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        free.try_reserve_exact(capacity)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for index in 0..capacity {
            pending.push(Pending {
                ticket: 0,
                status: None,
            });
            free.push(index);
        }
        Ok(Self {
            ring,
            ready,
            progress,
            port,
            state: Mutex::new(State {
                pending,
                free,
                next: 1,
                leader: false,
                armed: true,
                disconnected: false,
                aborting: false,
            }),
            changed: AtomicU32::new(0),
        })
    }
    pub(super) fn capacity(&self) -> usize {
        self.ring.capacity()
    }
    fn signal(&self) {
        // 只有本对象持有这些句柄；publish 后的通知不能因内存分配失败而丢失。
        self.ready
            .signal(0, signals::SIGNALED)
            .expect("validated block doorbell must remain usable");
    }
    fn abort(&self, state: &mut State) {
        if !state.aborting {
            state.aborting = true;
            self.ring.request_abort();
            self.signal();
        }
    }
    fn collect(&self, state: &mut State) -> usize {
        let mut responses = [[0; 2]; MAX_BATCH];
        let mut total = 0;
        loop {
            let count = match self.ring.take_responses(&mut responses) {
                Ok(count) => count,
                Err(_) => {
                    self.abort(state);
                    break;
                }
            };
            if count == 0 {
                break;
            }
            for &[ticket, status] in &responses[..count] {
                let index = ticket as usize & (self.ring.capacity() - 1);
                let pending = &mut state.pending[index];
                if ticket == 0
                    || pending.ticket != ticket
                    || pending.status.is_some()
                    || status > u32::MAX as u64
                    || (status as u32 as i32) > 0
                {
                    self.abort(state);
                } else {
                    pending.status = Some(status as u32 as i32);
                }
            }
            total += count;
            if state.aborting {
                break;
            }
        }
        state.disconnected |= self.ring.closed();
        total
    }
    fn notify(&self) {
        self.changed.fetch_add(1, Ordering::Release);
        futex::wake(&self.changed, u32::MAX, 0);
    }
    /// 发布一批已校验的请求；全部完成前保留 credits，即使其中一个请求已失败。
    pub(super) fn transfer(
        &self,
        count: usize,
        mut record: impl FnMut(usize) -> Request,
    ) -> Result<(), Status> {
        if count == 0 || count > self.ring.capacity() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        // 常见 virtqueue 的全部在途身份放栈上；更大的协商容量只在发表前准备一次有界暂存。
        // 不按服务端每次 64 项的领取窗口截断客户端提交，否则单个调用者无法填满较深的硬件队列。
        let mut inline = [0; MAX_BATCH * 2];
        let mut overflow = Vec::new();
        let tickets = if count <= inline.len() {
            &mut inline[..count]
        } else {
            overflow
                .try_reserve_exact(count)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            overflow.resize(count, 0);
            overflow.as_mut_slice()
        };
        loop {
            let mut state = self.state.lock();
            state.disconnected |= self.ring.closed();
            if state.disconnected {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            if state.aborting {
                return Err(native_abi::ERR_BAD_STATE);
            }
            if state.free.len() >= count {
                let next = state
                    .next
                    .checked_add(count as u64)
                    .ok_or(native_abi::ERR_NO_RESOURCES)?;
                next.checked_mul(self.ring.capacity() as u64)
                    .ok_or(native_abi::ERR_NO_RESOURCES)?;
                if let Err(error) = self.ring.submit(count, |position| {
                    let mut record = record(position);
                    let index = state.free.pop().expect("reserved block queue credit");
                    let ticket = state.next * self.ring.capacity() as u64 + index as u64;
                    state.next += 1;
                    state.pending[index] = Pending {
                        ticket,
                        status: None,
                    };
                    tickets[position] = ticket;
                    record[0] = ticket;
                    if position + 1 == count {
                        record[1] |= queue::GROUP_LAST;
                    }
                    record
                }) {
                    // push 的全部检查在回调前完成；失败时既没领 credits，也没有已发表请求。
                    self.abort(&mut state);
                    self.notify();
                    return Err(error);
                }
                drop(state);
                self.signal();
                break;
            }
            drop(state);
            self.progress(|state| state.free.len() >= count || state.aborting);
        }
        loop {
            let mut state = self.state.lock();
            state.disconnected |= self.ring.closed();
            let done = tickets[..count].iter().all(|&ticket| {
                state.pending[ticket as usize & (self.ring.capacity() - 1)]
                    .status
                    .is_some()
            });
            if done || state.disconnected {
                let mut error = if state.disconnected {
                    Some(native_abi::ERR_PEER_CLOSED)
                } else if state.aborting {
                    Some(native_abi::ERR_BAD_STATE)
                } else {
                    None
                };
                for &ticket in &tickets[..count] {
                    let index = ticket as usize & (self.ring.capacity() - 1);
                    let pending = &mut state.pending[index];
                    if let Some(status) = pending.status
                        && status != native_abi::OK
                    {
                        error.get_or_insert(status);
                    }
                    *pending = Pending {
                        ticket: 0,
                        status: None,
                    };
                    state.free.push(index);
                }
                drop(state);
                // 本地 credits 归还也要唤醒 port 上的分派者，它可能正代表另一个提交者等空位。
                self.progress
                    .signal(0, signals::SIGNALED)
                    .expect("validated block completion event must remain usable");
                self.notify();
                return error.map_or(Ok(()), Err);
            }
            drop(state);
            self.progress(|state| {
                tickets[..count].iter().all(|&ticket| {
                    state.pending[ticket as usize & (self.ring.capacity() - 1)]
                        .status
                        .is_some()
                })
            });
        }
    }
    fn progress(&self, ready: impl Fn(&State) -> bool) {
        let expected = self.changed.load(Ordering::Acquire);
        let leader = {
            let mut state = self.state.lock();
            if ready(&state) || state.disconnected {
                return;
            }
            if state.leader {
                false
            } else {
                state.leader = true;
                true
            }
        };
        if !leader {
            // 等待失败也只重新检查谓词，不能把已提交但未完成的 DMA 当作取消成功。
            let _ = futex::wait(&self.changed, expected, DEADLINE_INFINITE, 0);
            return;
        }
        self.drive(&ready);
        self.state.lock().leader = false;
        self.notify();
    }
    fn drive(&self, ready: &impl Fn(&State) -> bool) {
        loop {
            // clear -> drain -> arm -> wait；发布与任何一步交错都会留下完成记录或未消费的通知。
            self.progress
                .signal(signals::SIGNALED, 0)
                .expect("validated block completion event must remain usable");
            let mut state = self.state.lock();
            if self.collect(&mut state) != 0 || state.disconnected || ready(&state) {
                return;
            }
            if !state.armed {
                match self.progress.wait_async(&self.port, 1, signals::SIGNALED) {
                    Ok(()) => state.armed = true,
                    Err(_) => {
                        drop(state);
                        // 仅在内核无法准备等待资源时限速重试；保有所有在途 credits，绝不提前归还 DMA 缓冲。
                        let _ = native_runtime::system::sleep_until(
                            native_runtime::system::deadline_after(1_000_000),
                        );
                        continue;
                    }
                }
            }
            drop(state);
            match self.port.wait(DEADLINE_INFINITE) {
                Ok(packet) => {
                    let mut state = self.state.lock();
                    match packet.key {
                        0 => state.disconnected = true,
                        1 => state.armed = false,
                        _ => self.abort(&mut state),
                    }
                }
                Err(_) => {
                    // packet 未交付时 Port 保留它；继续检查环，错误不是 DMA 的完成证明。
                    let _ = native_runtime::system::sleep_until(
                        native_runtime::system::deadline_after(1_000_000),
                    );
                }
            }
        }
    }
}
