//! futex：按地址等待与唤醒。
//!
//! key 默认是（地址空间, 虚拟地址）；`futex::SHARED` 时解析为（VMO, 字节偏移），共享同一内存的不同
//! 地址空间因此看到同一个 futex。等待者按登记顺序排列（key 后附递增序号），唤醒按 FIFO。
//!
//! 1. wait 在表锁内读取并比较用户值、登记等待者，再在锁外睡眠；waker 在同一把锁内摘除并唤醒，
//!    因此“比较后、睡眠前”到达的唤醒不会丢失。
//! 2. 超时或被打断的等待者醒来后在表锁内摘除自身；已被唤醒者已经不在表中。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use native_abi::Status;
use spin::Mutex;

use super::Call;
use crate::execution::sync::{TaskEvent, TaskMutex, WaitResult};
use fallible_tree::FallibleMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FutexKey {
    Private { space: u64, address: usize },
    Shared { vmo: u64, offset: u64 },
}

struct Waiter {
    event: TaskEvent,
    // FLAG: 已被 waker 摘除；醒来后据此区分唤醒与超时。
    woken: AtomicBool,
    // OWNER: 当前登记位置；requeue 在表锁内改写，超时摘除时据此定位。
    slot: Mutex<(FutexKey, u64)>,
}

// OWNER: 全部 futex 等待者的唯一索引；比较用户值时持有（可能缺页），因此是可睡眠锁。
static TABLE: TaskMutex<FallibleMap<(FutexKey, u64), Arc<Waiter>>> = TaskMutex::new(FallibleMap::new());
// OWNER: 登记序号；Relaxed，只提供 FIFO 顺序与唯一性。
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

fn key(call: &Call<'_>, address: usize, options: u32) -> Result<FutexKey, Status> {
    if !address.is_multiple_of(4) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let space = call.thread.address_space().ok_or(native_abi::ERR_BAD_STATE)?;
    match options {
        0 => Ok(FutexKey::Private {
            space: space.futex_owner(address),
            address,
        }),
        native_abi::futex::SHARED => {
            let (vmo, offset) = space
                .resolve_object(address)
                .map_err(|_| native_abi::ERR_INVALID_POINTER)?;
            Ok(FutexKey::Shared { vmo, offset })
        }
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}

pub(super) fn wait(call: &Call<'_>) -> Result<(), Status> {
    let address = call.arg(0);
    let key = key(call, address, call.arg_u32(3))?;
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    let waiter = Arc::try_new(Waiter {
        event: TaskEvent::new(),
        woken: AtomicBool::new(false),
        slot: Mutex::new((key, ticket)),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let entry = FallibleMap::try_prepare((key, ticket), waiter.clone()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    {
        let mut table = TABLE.lock().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let value: u32 = call.read(address)?;
        if value != call.arg_u32(1) {
            return Err(native_abi::ERR_BAD_STATE);
        }
        table.commit_vacant(entry);
    }
    let result = match super::deadline(call.arg(2)) {
        Some(deadline) => waiter.event.wait_interruptible_until(deadline),
        None => waiter.event.wait_interruptible(),
    };
    if waiter.woken.load(Ordering::Acquire) {
        return Ok(());
    }
    {
        let mut table = TABLE.lock().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let slot = *waiter.slot.lock();
        drop(table.remove(&slot));
    }
    // 摘除前可能刚被唤醒：以 woken 为准，避免把成功的唤醒报告为超时。
    if waiter.woken.load(Ordering::Acquire) {
        return Ok(());
    }
    Err(match result {
        WaitResult::TimedOut => native_abi::ERR_TIMED_OUT,
        WaitResult::OutOfMemory => native_abi::ERR_NO_MEMORY,
        WaitResult::Woken | WaitResult::Interrupted => native_abi::ERR_CANCELED,
    })
}

/// 在表锁内摘除并唤醒至多 `count` 个等待者。
fn wake_locked(table: &mut FallibleMap<(FutexKey, u64), Arc<Waiter>>, key: FutexKey, count: usize) -> u32 {
    let mut woken = 0;
    while (woken as usize) < count {
        let next = table.iter_from(&(key, 0)).next().map(|(slot, _)| *slot).filter(|slot| slot.0 == key);
        let Some(slot) = next else { break };
        let waiter = table.remove(&slot).expect("futex waiter vanished");
        waiter.woken.store(true, Ordering::Release);
        waiter.event.signal();
        woken += 1;
    }
    woken
}

pub(super) fn wake(call: &Call<'_>) -> Result<(), Status> {
    let key = key(call, call.arg(0), call.arg_u32(2))?;
    let woken = {
        let mut table = TABLE.lock().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        wake_locked(&mut table, key, call.arg(1))
    };
    call.write_optional(call.arg(3), &woken)
}

pub(super) fn requeue(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(5);
    let source = key(call, call.arg(0), options)?;
    let target = key(call, call.arg(3), options)?;
    let mut table = TABLE.lock().map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let value: u32 = call.read(call.arg(0))?;
    if value != call.arg_u32(2) {
        return Err(native_abi::ERR_BAD_STATE);
    }
    wake_locked(&mut table, source, call.arg(1));
    let mut moved = 0;
    while moved < call.arg(4) {
        let next = table.iter_from(&(source, 0)).next().map(|(slot, _)| *slot).filter(|slot| slot.0 == source);
        let Some(slot) = next else { break };
        let mut entry = table.take_entry(&slot).expect("futex waiter vanished");
        let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
        *entry.value().slot.lock() = (target, ticket);
        entry.set_key((target, ticket));
        table.commit_vacant(entry);
        moved += 1;
    }
    Ok(())
}
