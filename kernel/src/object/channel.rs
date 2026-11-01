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
use core::sync::atomic::{AtomicU32, Ordering};

use native_abi::{ChannelActual, Status, signals};
use spin::Mutex;

use super::handle_table::HandleEntry;
use super::signals::SignalState;
use crate::execution::sync::{SpareStorage, lock_with_spare};
use crate::execution::sync::{TaskEvent, WaitResult};

/// 单个端点允许积压的消息数；超过时写入返回 `ERR_NO_RESOURCES`，防止失控的发送者耗尽内存。
const MAX_PENDING_MESSAGES: usize = 4096;
const KERNEL_TXID_BIT: u32 = 1 << 31;

/// 一条 channel 消息。
pub(crate) struct Message {
    /// 消息字节。
    pub(crate) bytes: Vec<u8>,
    /// 随消息转移的句柄。
    pub(crate) handles: Vec<HandleEntry>,
}

impl Message {
    fn actual(&self) -> ChannelActual {
        ChannelActual {
            bytes: self.bytes.len() as u32,
            handles: self.handles.len() as u32,
        }
    }

    fn txid(&self) -> Option<u32> {
        let bytes = self.bytes.get(..native_abi::CHANNEL_TXID_BYTES)?;
        Some(u32::from_ne_bytes(bytes.try_into().expect("txid width")))
    }
}

struct CallWaiter {
    txid: u32,
    event: TaskEvent,
    // OWNER: 回复只由写入方或端点关闭方在 channel state 锁内发布一次，等待者在摘除登记后读取。
    reply: Mutex<Option<Result<Message, Status>>>,
}

#[derive(Default)]
struct Side {
    inbound: VecDeque<Message>,
    open: bool,
    calls: Vec<Arc<CallWaiter>>,
}

struct ChannelShared {
    // OWNER: 两个端点的队列、开闭与事务登记同锁发布；拆开会让关闭与写入交错后把消息留在已关闭端点。
    state: Mutex<[Side; 2]>,
    signals: [SignalState; 2],
    next_txid: AtomicU32,
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
            open: true,
            calls: Vec::new(),
        };
        let shared = Arc::try_new(ChannelShared {
            state: Mutex::new([side(), side()]),
            signals: [
                SignalState::new(signals::WRITABLE),
                SignalState::new(signals::WRITABLE),
            ],
            next_txid: AtomicU32::new(0),
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
    pub(crate) fn write(&self, message: Message) -> Result<(), (Status, Message)> {
        let peer = self.peer();
        let Ok(mut state) =
            lock_with_spare(&self.shared.state, |_| 1, |state| &mut state[peer].inbound)
        else {
            return Err((native_abi::ERR_NO_MEMORY, message));
        };
        self.publish(&mut state, message)
    }

    fn publish(&self, state: &mut [Side; 2], message: Message) -> Result<(), (Status, Message)> {
        let peer = self.peer();
        if !state[peer].open {
            return Err((native_abi::ERR_PEER_CLOSED, message));
        }
        if let Some(txid) = message.txid()
            && let Some(position) = state[peer].calls.iter().position(|call| call.txid == txid)
        {
            let call = state[peer].calls.swap_remove(position);
            *call.reply.lock() = Some(Ok(message));
            call.event.signal();
            return Ok(());
        }
        if state[peer].inbound.len() >= MAX_PENDING_MESSAGES {
            return Err((native_abi::ERR_NO_RESOURCES, message));
        }
        state[peer].inbound.push_back(message);
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
        let peer_open = state[self.peer()].open;
        let side = &mut state[self.side];
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
        if front.bytes.len() > bytes_capacity || front.handles.len() > handles_capacity {
            return Err(ReadError {
                status: native_abi::ERR_BUFFER_TOO_SMALL,
                actual: front.actual(),
            });
        }
        let message = side.inbound.pop_front().expect("front message exists");
        if side.inbound.is_empty() {
            self.shared.signals[self.side].update(signals::READABLE, 0);
        }
        Ok(message)
    }

    /// 写请求并等待匹配事务号的回复。
    ///
    /// # Parameters
    /// - `message`: 请求，至少 4 字节；首 4 字节被覆盖为内核事务号。
    /// - `deadline_ns`: 单调时钟绝对截止时间。
    ///
    /// # Errors
    /// `ERR_INVALID_ARGS`（不足 4 字节）、写入错误、`ERR_TIMED_OUT`、`ERR_CANCELED`（进程终止）、
    /// `ERR_PEER_CLOSED`。写入失败时请求交还调用者。
    pub(crate) fn call(
        &self,
        mut message: Message,
        deadline_ns: Option<u64>,
    ) -> Result<Message, (Status, Option<Message>)> {
        if message.bytes.len() < native_abi::CHANNEL_TXID_BYTES {
            return Err((native_abi::ERR_INVALID_ARGS, Some(message)));
        }
        let txid = self.shared.next_txid.fetch_add(1, Ordering::Relaxed) | KERNEL_TXID_BIT;
        message.bytes[..native_abi::CHANNEL_TXID_BYTES].copy_from_slice(&txid.to_ne_bytes());
        let waiter = match Arc::try_new(CallWaiter {
            txid,
            event: TaskEvent::new(),
            reply: Mutex::new(None),
        }) {
            Ok(waiter) => waiter,
            Err(_) => return Err((native_abi::ERR_NO_MEMORY, Some(message))),
        };
        let peer = self.peer();
        let own = self.side;
        // 请求入队与事务登记需要两处空位；交替扩容直到同一临界区内两者都满足。
        let mut state = loop {
            let Ok(state) =
                lock_with_spare(&self.shared.state, |_| 1, |state| &mut state[own].calls)
            else {
                return Err((native_abi::ERR_NO_MEMORY, Some(message)));
            };
            if has_spare(&state[peer].inbound) {
                break state;
            }
            drop(state);
            let Ok(state) =
                lock_with_spare(&self.shared.state, |_| 1, |state| &mut state[peer].inbound)
            else {
                return Err((native_abi::ERR_NO_MEMORY, Some(message)));
            };
            if has_spare(&state[own].calls) {
                break state;
            }
        };
        if let Err((status, message)) = self.publish(&mut state, message) {
            return Err((status, Some(message)));
        }
        state[own].calls.push(waiter.clone());
        drop(state);

        let result = match deadline_ns {
            Some(deadline) => waiter.event.wait_interruptible_until(deadline),
            None => waiter.event.wait_interruptible(),
        };
        {
            let mut state = self.shared.state.lock();
            if let Some(position) = state[own]
                .calls
                .iter()
                .position(|call| Arc::ptr_eq(call, &waiter))
            {
                state[own].calls.swap_remove(position);
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
            state[self.side].open = false;
            let retired = core::mem::take(&mut state[self.side].inbound);
            for call in state[peer].calls.drain(..) {
                *call.reply.lock() = Some(Err(native_abi::ERR_PEER_CLOSED));
                call.event.signal();
            }
            self.shared.signals[peer].update(signals::WRITABLE, signals::PEER_CLOSED);
            retired
        };
        // 入站消息可能携带其他 channel 端点，其 Drop 会取得各自的锁，必须在本锁外释放。
        drop(retired);
    }
}
