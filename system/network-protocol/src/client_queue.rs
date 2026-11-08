//! 多调用者共享 SQ/CQ；当前调用者之一分派完成，其余等本地 epoch，不增加转发线程。
use super::*;
use core::sync::atomic::{AtomicU32, Ordering};
use native_runtime::native_abi::{DEADLINE_INFINITE, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Port, futex};
use queue::{CAPACITY, Queue, Record};

#[derive(Clone, Copy)]
struct Pending {
    ticket: u64,
    response: Option<Record>,
}
struct State {
    pending: [Pending; CAPACITY],
    free: u32,
    next: u64,
    // OWNER: 在本地 epoch 上等待领队交接的调用者数，受同一谓词锁保护；无人等待时不进 futex wake。
    followers: usize,
    // FLAG: 唯一 CQ consumer/port waiter；否则多个等待者清事件会丢失彼此的唤醒。
    leader: bool,
    // FLAG: progress 的一次性 port 登记尚未消费，防止重复武装积压 packet。
    armed: bool,
    // FLAG: 对端关闭或协议损坏后的终态；此后绝不复用载荷槽。
    disconnected: bool,
}
pub(super) struct ClientQueue {
    ring: Queue,
    ready: Event,
    progress: Event,
    port: Port,
    // OWNER: SQ producer/CQ consumer、ticket 与载荷槽寿命由本端一个锁串行；等待不持锁。
    state: Mutex<State>,
    // OWNER: 本地完成/credit 交接序号；先快照再查谓词，避免交接时漏唤醒。
    changed: AtomicU32,
}
impl ClientQueue {
    pub(super) fn connect(channel: &Channel) -> Result<Self, Status> {
        let mut reply = service_rpc::call(channel, &Writer::request(ordinal::QUEUE), Vec::new())?;
        if reply.reader().u64()? != queue::MAGIC {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let ring = Queue::open(Vmo::from_handle(take_handle(&mut reply.handles, 0)?))?;
        let ready = Event::from_handle(take_handle(&mut reply.handles, 1)?);
        let progress = Event::from_handle(take_handle(&mut reply.handles, 2)?);
        ready.signal(0, 0)?;
        progress.signal(0, 0)?;
        let port = Port::create()?;
        channel.wait_async(&port, 0, signals::PEER_CLOSED)?;
        progress.wait_async(&port, 1, signals::SIGNALED)?;
        Ok(Self {
            ring,
            ready,
            progress,
            port,
            state: Mutex::new(State {
                pending: [Pending {
                    ticket: 0,
                    response: None,
                }; CAPACITY],
                free: (1 << CAPACITY) - 1,
                next: 1,
                followers: 0,
                leader: false,
                armed: true,
                disconnected: false,
            }),
            changed: AtomicU32::new(0),
        })
    }
    fn signal(&self) {
        self.ready
            .signal(0, signals::SIGNALED)
            .expect("validated socket doorbell must remain usable");
    }
    fn notify(&self, followers: bool) {
        self.changed.fetch_add(1, Ordering::Release);
        if followers {
            futex::wake(&self.changed, u32::MAX, 0);
        }
    }
    fn fail(&self, state: &mut State) {
        state.disconnected = true;
        self.ring.abort();
        self.signal();
    }
    fn collect(&self, state: &mut State) -> bool {
        let mut changed = false;
        loop {
            match self.ring.response() {
                Ok(Some(response)) => {
                    let ticket = response[0];
                    let pending = &mut state.pending[ticket as usize & (CAPACITY - 1)];
                    if ticket == 0
                        || pending.ticket != ticket
                        || pending.response.is_some()
                        || (response[1] != 0
                            && u8::try_from(response[1])
                                .ok()
                                .and_then(SocketError::from_code)
                                .is_none())
                        || response[12..].iter().any(|&value| value != 0)
                    {
                        self.fail(state);
                        break;
                    }
                    pending.response = Some(response);
                    changed = true;
                }
                Ok(None) => break,
                Err(_) => {
                    self.fail(state);
                    break;
                }
            }
        }
        state.disconnected |= self.ring.closed();
        changed
    }
    pub(super) fn transfer(
        &self,
        mut record: Record,
        input: &[u8],
    ) -> Result<(Record, Vec<u8>), Status> {
        if input.len() > TRANSFER_LIMIT {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let ticket = loop {
            let mut state = self.state.lock();
            state.disconnected |= self.ring.closed();
            if state.disconnected {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            if state.free != 0 {
                let next = state
                    .next
                    .checked_add(1)
                    .ok_or(native_abi::ERR_NO_RESOURCES)?;
                next.checked_mul(CAPACITY as u64)
                    .ok_or(native_abi::ERR_NO_RESOURCES)?;
                let index = state.free.trailing_zeros() as usize;
                let ticket = state.next * CAPACITY as u64 + index as u64;
                record[0] = ticket;
                self.ring.write_payload(ticket, input)?;
                // 所有已占槽直到调用者复制结束才归还，故 SQ 不可能先于 credits 耗尽。
                if let Err(status) = self.ring.submit(record) {
                    self.fail(&mut state);
                    let followers = state.followers != 0;
                    drop(state);
                    self.notify(followers);
                    return Err(status);
                }
                state.next = next;
                state.free &= !(1 << index);
                state.pending[index] = Pending {
                    ticket,
                    response: None,
                };
                drop(state);
                self.signal();
                break ticket;
            }
            drop(state);
            self.progress(|state| state.free != 0);
        };
        loop {
            let mut state = self.state.lock();
            let index = ticket as usize & (CAPACITY - 1);
            if let Some(response) = state.pending[index].response {
                // CQ 消费只释放记录槽。这里复制完载荷才释放 credit，服务端不能提前覆盖。
                let mut output = Vec::new();
                let result = if record[1] == socket_io::RECEIVE && response[1] == 0 {
                    if response[2] > record[2] || response[3] < response[2] {
                        Err(native_abi::ERR_BAD_STATE)
                    } else {
                        self.ring
                            .read_payload(ticket, response[2] as usize, &mut output)
                    }
                } else if record[1] == socket_io::SEND
                    && response[1] == 0
                    && response[2] > input.len() as u64
                {
                    Err(native_abi::ERR_BAD_STATE)
                } else {
                    Ok(())
                };
                if result == Err(native_abi::ERR_BAD_STATE) {
                    self.fail(&mut state);
                }
                state.pending[index] = Pending {
                    ticket: 0,
                    response: None,
                };
                state.free |= 1 << index;
                let leader = state.leader;
                let followers = state.followers != 0;
                drop(state);
                // 另一个 port leader 可能正在等 credit；单调用者路径不制造自己的多余通知。
                if leader {
                    self.progress
                        .signal(0, signals::SIGNALED)
                        .expect("validated socket progress must remain usable");
                }
                self.notify(followers);
                result?;
                return Ok((response, output));
            }
            if state.disconnected {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            drop(state);
            self.progress(|state| state.pending[index].response.is_some());
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
                state.followers += 1;
                false
            } else {
                state.leader = true;
                true
            }
        };
        if !leader {
            let _ = futex::wait(&self.changed, expected, DEADLINE_INFINITE, 0);
            self.state.lock().followers -= 1;
            return;
        }
        self.drive(&ready);
        let followers = {
            let mut state = self.state.lock();
            state.leader = false;
            state.followers != 0
        };
        self.notify(followers);
    }
    fn drive(&self, ready: &impl Fn(&State) -> bool) {
        loop {
            // clear → drain → arm → wait；任一交错都会留下记录或未消费通知。
            self.progress
                .signal(signals::SIGNALED, 0)
                .expect("validated socket progress must remain usable");
            let mut state = self.state.lock();
            if self.collect(&mut state) || state.disconnected || ready(&state) {
                return;
            }
            if !state.armed {
                if self
                    .progress
                    .wait_async(&self.port, 1, signals::SIGNALED)
                    .is_err()
                {
                    self.fail(&mut state);
                    return;
                }
                state.armed = true;
            }
            drop(state);
            match self.port.wait(DEADLINE_INFINITE) {
                Ok(packet) => {
                    let mut state = self.state.lock();
                    match packet.key {
                        0 => state.disconnected = true,
                        1 => state.armed = false,
                        _ => self.fail(&mut state),
                    }
                }
                Err(_) => {
                    self.fail(&mut self.state.lock());
                    return;
                }
            }
        }
    }
}
