//! Channel：双向消息端点对。
//!
//! 1. 每个端点拥有自己的入站队列；写入进入对端队列并置对端 `READABLE`。
//! 2. 端点最后一个引用释放时丢弃其入站队列，对端清 `WRITABLE`、置 `PEER_CLOSED`，
//!    对端仍在等待回复的 `call` 以 `ERR_PEER_CLOSED` 结束。
//! 3. `call` 在同一临界区写请求并登记事务号；对端写入首 4 字节匹配该事务号的消息时
//!    直接交给等待者而不进入队列（Zircon `zx_channel_call` 语义）。内核事务号最高位为 1，
//!    与用户自定义事务号不相交。
//!
//! 锁序：channel state → signal state → port queue。

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use native_abi::{ChannelActual, Status, signals};
use spin::Mutex;

use super::handle_table::HandleEntry;
use super::signals::SignalState;
use crate::execution::sync::{SpareStorage, lock_with_spare};
use crate::execution::sync::{TaskEvent, WaitResult};

/// 单个端点允许积压的消息所占字节（含每条消息的固定开销）；超过时写入返回 `ERR_NO_RESOURCES`，
/// 防止失控的发送者耗尽内存。按字节而非条数限额：条数上限对 64 KiB 的整消息会放大到数百 MiB，
/// 对小消息又过紧（Fuchsia/Linux 套接字缓冲同样按字节记账）。
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const KERNEL_TXID_BIT: u32 = 1 << 31;

mod call;
mod message;
pub(in crate::object) use call::CallSlot;
use message::Buffers;
pub(crate) use message::Message;

struct CallWaiter {
    txid: u32,
    event: TaskEvent,
    // OWNER: 回复只由写入方或端点关闭方在 channel state 锁内发布一次，等待者在摘除登记后读取。
    reply: Mutex<Option<Result<Message, Status>>>,
}

#[derive(Default)]
struct Side {
    inbound: VecDeque<Message>,
    // OWNER: `inbound` 中全部消息的 `charge` 之和，与队列同锁增减；缺失时字节上限无法判定。
    queued_bytes: usize,
    open: bool,
    calls: Vec<Arc<CallWaiter>>,
}

struct ChannelShared {
    // OWNER: 两个端点的队列、开闭与事务登记同锁发布；拆开会让关闭与写入交错后把消息留在已关闭端点。
    state: Mutex<ChannelState>,
    signals: [SignalState; 2],
}

struct ChannelState {
    sides: [Side; 2],
    // OWNER: 唯一空消息存储，与队列同锁移交；不含句柄，不保留超过一条消息的容量。
    spare: Buffers,
    next_txid: u32,
    // CACHE: (next_txid, free_until) 已证明无在途事务；新调用只在游标后方登记，退休只扩大空隙。
    // 不保留空隙证明就必须每次扫描全部等待者；环绕或碰到旧边界时才重新求证。
    free_until: u32,
}

/// channel 端点。
pub(crate) struct Channel {
    koid: u64,
    side: usize,
    shared: Arc<ChannelShared>,
}

/// `read` 失败的原因与（缓冲区不足时）所需大小。
pub(crate) struct ReadError {
    /// 失败状态。
    pub(crate) status: Status,
    /// `ERR_BUFFER_TOO_SMALL` 时队首消息的大小。
    pub(crate) actual: ChannelActual,
}

impl Channel {
    /// 创建一对相连的端点。
    ///
    /// # Errors
    /// 分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn create_pair() -> Result<(Self, Self), Status> {
        let side = || Side {
            inbound: VecDeque::new(),
            queued_bytes: 0,
            open: true,
            calls: Vec::new(),
        };
        let shared = Arc::try_new(ChannelShared {
            state: Mutex::new(ChannelState {
                sides: [side(), side()],
                spare: Buffers::default(),
                next_txid: 0,
                free_until: KERNEL_TXID_BIT,
            }),
            signals: [
                SignalState::new(signals::WRITABLE),
                SignalState::new(signals::WRITABLE),
            ],
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Ok((
            Self {
                koid: super::next_koid(),
                side: 0,
                shared: shared.clone(),
            },
            Self {
                koid: super::next_koid(),
                side: 1,
                shared,
            },
        ))
    }

    /// 本端点当前的 signal。
    pub(crate) fn signals_active(&self) -> u32 {
        self.shared.signals[self.side].active()
    }

    fn peer(&self) -> usize {
        1 - self.side
    }

    /// 写入一条消息。
    ///
    /// # Errors
    /// `ERR_PEER_CLOSED`、`ERR_NO_RESOURCES`、`ERR_NO_MEMORY`；失败时消息交还调用者在锁外释放。
    pub(crate) fn write(&self, mut message: Message) -> Result<(), (Status, Message)> {
        message.bind(&self.shared);
        let peer = self.peer();
        let Ok(mut state) = lock_with_spare(
            &self.shared.state,
            |_| 1,
            |state| &mut state.sides[peer].inbound,
        ) else {
            return Err((native_abi::ERR_NO_MEMORY, message));
        };
        self.publish(&mut state, message)
    }

    fn publish(&self, state: &mut ChannelState, message: Message) -> Result<(), (Status, Message)> {
        let peer = self.peer();
        if !state.sides[peer].open {
            return Err((native_abi::ERR_PEER_CLOSED, message));
        }
        if let Some(txid) = message.txid()
            && let Some(position) = state.sides[peer]
                .calls
                .iter()
                .position(|call| call.txid == txid)
        {
            let call = state.sides[peer].calls.swap_remove(position);
            *call.reply.lock() = Some(Ok(message));
            // 同步调用的 caller 接着消费本回复；留在 reply CPU 可复用即将再次等待请求的 server 时间片。
            // 不绕过 waiter identity、affinity 或正常 CFS；peer close 等批量通知仍用普通 placement。
            call.event.signal_on_current_cpu();
            return Ok(());
        }
        if message.charge() > MAX_PENDING_BYTES - state.sides[peer].queued_bytes {
            return Err((native_abi::ERR_NO_RESOURCES, message));
        }
        state.sides[peer].queued_bytes += message.charge();
        state.sides[peer].inbound.push_back(message);
        self.shared.signals[peer].update(0, signals::READABLE);
        Ok(())
    }

    /// 读取队首消息。
    ///
    /// # Errors
    /// `ERR_SHOULD_WAIT`（空）、`ERR_PEER_CLOSED`（空且对端关闭）、`ERR_BUFFER_TOO_SMALL`（消息保留）。
    pub(crate) fn read(
        &self,
        bytes_capacity: usize,
        handles_capacity: usize,
    ) -> Result<Message, ReadError> {
        let mut state = self.shared.state.lock();
        let peer_open = state.sides[self.peer()].open;
        let side = &mut state.sides[self.side];
        let Some(front) = side.inbound.front() else {
            let status = if peer_open {
                native_abi::ERR_SHOULD_WAIT
            } else {
                native_abi::ERR_PEER_CLOSED
            };
            return Err(ReadError {
                status,
                actual: ChannelActual::default(),
            });
        };
        if front.actual().bytes as usize > bytes_capacity
            || front.actual().handles as usize > handles_capacity
        {
            return Err(ReadError {
                status: native_abi::ERR_BUFFER_TOO_SMALL,
                actual: front.actual(),
            });
        }
        let message = side.inbound.pop_front().expect("front message exists");
        side.queued_bytes -= message.charge();
        if side.inbound.is_empty() {
            self.shared.signals[self.side].update(signals::READABLE, 0);
        }
        Ok(message)
    }

    /// 写请求并等待匹配事务号的回复。
    ///
    /// # Parameters
    /// - `slot`: 当前原生线程独占的调用存储，只有撤销登记后才能复用。
    /// - `message`: 请求，至少 4 字节；首 4 字节被覆盖为内核事务号。
    /// - `deadline_ns`: 单调时钟绝对截止时间。
    ///
    /// # Errors
    /// `ERR_INVALID_ARGS`（不足 4 字节）、写入错误、`ERR_TIMED_OUT`、`ERR_CANCELED`（进程终止）、
    /// `ERR_PEER_CLOSED`。写入失败时请求交还调用者。
    pub(in crate::object) fn call(
        &self,
        slot: &CallSlot,
        mut message: Message,
        deadline_ns: Option<u64>,
    ) -> Result<Message, (Status, Option<Message>)> {
        message.bind(&self.shared);
        if message.bytes().len() < native_abi::CHANNEL_TXID_BYTES {
            return Err((native_abi::ERR_INVALID_ARGS, Some(message)));
        }
        let mut waiter = match slot.acquire() {
            Ok(waiter) => waiter,
            Err(status) => return Err((status, Some(message))),
        };
        let peer = self.peer();
        let own = self.side;
        // 请求入队与事务登记需要两处空位；交替扩容直到同一临界区内两者都满足。
        let mut state = loop {
            let Ok(state) = lock_with_spare(
                &self.shared.state,
                |_| 1,
                |state| &mut state.sides[own].calls,
            ) else {
                return Err((native_abi::ERR_NO_MEMORY, Some(message)));
            };
            if has_spare(&state.sides[peer].inbound) {
                break state;
            }
            drop(state);
            let Ok(state) = lock_with_spare(
                &self.shared.state,
                |_| 1,
                |state| &mut state.sides[peer].inbound,
            ) else {
                return Err((native_abi::ERR_NO_MEMORY, Some(message)));
            };
            if has_spare(&state.sides[own].calls) {
                break state;
            }
        };
        // 两端同锁分配并跳过仍登记的事务；低 31 位环绕不能命中另一侧的存活调用。
        let txid = match state.next_txid() {
            Ok(txid) => txid,
            Err(status) => return Err((status, Some(message))),
        };
        waiter.set_txid(txid);
        message.bytes_mut()[..native_abi::CHANNEL_TXID_BYTES].copy_from_slice(&txid.to_ne_bytes());
        if let Err((status, message)) = self.publish(&mut state, message) {
            return Err((status, Some(message)));
        }
        state.sides[own].calls.push(Arc::clone(&waiter));
        drop(state);

        let result = match deadline_ns {
            Some(deadline) => waiter.event.wait_interruptible_until(deadline),
            None => waiter.event.wait_interruptible(),
        };
        {
            let mut state = self.shared.state.lock();
            if let Some(position) = state.sides[own]
                .calls
                .iter()
                .position(|call| Arc::ptr_eq(call, &waiter))
            {
                state.sides[own].calls.swap_remove(position);
            }
        }
        if let Some(reply) = waiter.reply.lock().take() {
            return reply.map_err(|status| (status, None));
        }
        Err((
            match result {
                WaitResult::TimedOut => native_abi::ERR_TIMED_OUT,
                WaitResult::OutOfMemory => native_abi::ERR_NO_MEMORY,
                WaitResult::Interrupted | WaitResult::Woken => native_abi::ERR_CANCELED,
            },
            None,
        ))
    }
}

impl super::KernelObject for Channel {
    fn object_type(&self) -> native_abi::ObjectType {
        native_abi::ObjectType::Channel
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        use native_abi::rights;
        super::WAITABLE_RIGHTS | rights::READ | rights::WRITE | rights::SIGNAL
    }
    fn signals(&self) -> Option<&SignalState> {
        Some(&self.shared.signals[self.side])
    }
}

fn has_spare<T: SpareStorage>(storage: &T) -> bool {
    storage.length() < storage.spare_capacity()
}

impl Drop for Channel {
    fn drop(&mut self) {
        let peer = self.peer();
        let retired = {
            let mut state = self.shared.state.lock();
            state.sides[self.side].open = false;
            let retired = core::mem::take(&mut state.sides[self.side].inbound);
            let spare = core::mem::take(&mut state.spare);
            state.sides[self.side].queued_bytes = 0;
            for call in state.sides[peer].calls.drain(..) {
                *call.reply.lock() = Some(Err(native_abi::ERR_PEER_CLOSED));
                call.event.signal();
            }
            self.shared.signals[peer].update(signals::WRITABLE, signals::PEER_CLOSED);
            (retired, spare)
        };
        // 入站消息可能携带其他 channel 端点，其 Drop 会取得各自的锁，必须在本锁外释放。
        drop(retired);
    }
}
