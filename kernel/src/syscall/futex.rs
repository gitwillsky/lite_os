//! futex：按地址等待与唤醒。
//!
//! key 默认是（地址空间, 虚拟地址）；`futex::SHARED` 时解析为（VMO, 字节偏移），共享同一内存的不同
//! 地址空间因此看到同一个 futex。等待者按登记顺序排列（key 后附递增序号），唤醒按 FIFO。
//!
//! # 分桶
//!
//! 全系统等待者按 key 哈希分入固定数量的桶，每个桶一把可睡眠锁和一棵按（key, 序号）排序的树：
//! 不同 key 的 wait/wake 只在哈希碰撞时才互相等待。同一 key 永远落在同一个桶，FIFO 因此不变。
//!
//! 1. wait 在桶锁内读取并比较用户值、登记等待者，再在锁外睡眠；waker 在同一把桶锁内摘除并唤醒，
//!    因此“比较后、睡眠前”到达的唤醒不会丢失。
//! 2. 桶锁内只做不缺页的读取：翻译缺失时释放桶锁、在锁外用普通读取缺页（可能等待 pager），然后重试。
//!    缺失这一步会让一次文件缺页的 I/O 延迟串行化同桶（原先是全系统）所有 futex 操作。
//! 3. 超时或被打断的等待者醒来后在自己的桶内摘除自身；已被唤醒者已经不在表中。
//! 4. requeue 需要同时持有源桶与目标桶：按桶下标升序加锁，同一个桶只锁一次；等待者的当前登记位置
//!    （`slot`）只在持有其所在桶锁时改写，摘除路径据此在锁内复核。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use native_abi::Status;
use spin::{Mutex, Once};

use super::Call;
use crate::execution::sync::{TaskEvent, TaskMutex, TaskMutexGuard, WaitResult};
mod queue;
use queue::{FutexKey, Slot};

type Bucket = queue::Bucket<Arc<Waiter>>;
type BucketGuard = TaskMutexGuard<'static, Bucket>;

/// 每个 CPU 的桶数（2 的幂）；总桶数 = 它乘以 CPU 数再向上取 2 的幂（Linux futex 哈希的经验比例）。
/// 并发 key 数大致随 CPU 数增长，固定桶数会让多核下的碰撞率随核数上升。
const BUCKETS_PER_CPU: usize = 256;

struct Waiter {
    event: TaskEvent,
    // FLAG: 已被 waker 摘除；醒来后据此区分唤醒与超时。
    woken: AtomicBool,
    // OWNER: 当前登记位置；requeue 在持有所在桶锁时改写，超时摘除时在桶锁内复核。
    slot: Mutex<Slot>,
}

// OWNER: 全部 futex 等待者的唯一索引，按 key 哈希分桶；桶锁内不缺页（见模块说明），
// 但取地址空间锁可能短暂睡眠，因此是可睡眠锁。桶数在首次使用时按 CPU 数一次确定，之后不变——
// 变化会让同一 key 落入不同桶而丢失唤醒。
static TABLE: Once<Box<[TaskMutex<Bucket>]>> = Once::new();
// OWNER: 登记序号；Relaxed，只提供 FIFO 顺序与唯一性。
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

fn table() -> &'static [TaskMutex<Bucket>] {
    TABLE.call_once(|| {
        let buckets = (BUCKETS_PER_CPU * crate::execution::cpu::count()).next_power_of_two();
        let mut table = Vec::new();
        table
            .try_reserve_exact(buckets)
            .expect("futex bucket table allocation failed");
        table.extend((0..buckets).map(|_| TaskMutex::new(Bucket::new())));
        table.into_boxed_slice()
    })
}

fn bucket(key: FutexKey) -> usize {
    key.bucket(table().len())
}

fn lock(key: FutexKey) -> Result<BucketGuard, Status> {
    table()[bucket(key)]
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)
}

fn key(call: &Call<'_>, address: usize, options: u32) -> Result<FutexKey, Status> {
    if !address.is_multiple_of(4) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let space = call
        .thread
        .address_space()
        .ok_or(native_abi::ERR_BAD_STATE)?;
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

/// 桶锁内读取 futex 字；翻译缺失返回 `None`，调用者必须放锁、`fault_in` 后重试。
fn read_word(call: &Call<'_>, address: usize) -> Result<Option<u32>, Status> {
    call.thread
        .address_space()
        .ok_or(native_abi::ERR_BAD_STATE)?
        .read_resident_word(address)
        .map_err(|_| native_abi::ERR_INVALID_POINTER)
}

/// 在没有任何 futex 锁的情况下缺页：可能等待 pager。
fn fault_in(call: &Call<'_>, address: usize) -> Result<(), Status> {
    call.read::<u32>(address).map(drop)
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
    let mut entry = Some(
        Bucket::prepare((key, ticket), waiter.clone()).map_err(|_| native_abi::ERR_NO_MEMORY)?,
    );
    loop {
        let mut table = lock(key)?;
        match read_word(call, address)? {
            Some(value) if value != call.arg_u32(1) => return Err(native_abi::ERR_BAD_STATE),
            Some(_) => {
                table.enqueue(entry.take().expect("futex waiter committed twice"));
                break;
            }
            None => {
                drop(table);
                fault_in(call, address)?;
            }
        }
    }
    let result = match super::deadline(call.arg(2)) {
        Some(deadline) => waiter.event.wait_interruptible_until(deadline),
        None => waiter.event.wait_interruptible(),
    };
    if waiter.woken.load(Ordering::Acquire) {
        return Ok(());
    }
    remove(&waiter)?;
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

/// 把仍在表中的等待者摘除。登记位置可能被并发 requeue 改到另一个桶：在桶锁内复核后才信任它。
fn remove(waiter: &Arc<Waiter>) -> Result<(), Status> {
    loop {
        let slot = *waiter.slot.lock();
        let mut table = lock(slot.0)?;
        if *waiter.slot.lock() != slot {
            continue;
        }
        drop(table.remove(&slot));
        return Ok(());
    }
}

/// 在桶锁内摘除并唤醒至多 `count` 个等待者，返回实际唤醒数。
///
/// 唤醒只发布 woken 标志并触发等待者的 `TaskEvent`，不等待也不分配。
fn wake_locked(table: &mut Bucket, key: FutexKey, count: usize) -> u32 {
    queue::wake_locked(table, key, count, wake_waiter)
}

/// requeue 对真实等待者的回调：唤醒、分配序号、改写登记位置。
struct WaiterHooks;

impl queue::RequeueHooks<Arc<Waiter>> for WaiterHooks {
    fn wake(&mut self, waiter: Arc<Waiter>) {
        wake_waiter(waiter);
    }
    fn next_ticket(&mut self) -> u64 {
        NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
    }
    fn moved(&mut self, waiter: &Arc<Waiter>, position: Slot) {
        *waiter.slot.lock() = position;
    }
}

fn wake_waiter(waiter: Arc<Waiter>) {
    waiter.woken.store(true, Ordering::Release);
    waiter.event.signal();
}

pub(super) fn wake(call: &Call<'_>) -> Result<(), Status> {
    let key = key(call, call.arg(0), call.arg_u32(2))?;
    let mut table = lock(key)?;
    let woken = wake_locked(&mut table, key, call.arg(1));
    drop(table);
    call.write_optional(call.arg(3), &woken)
}

/// 按桶下标升序锁住源、目标桶；同一个桶只锁一次（目标为 `None`）。
///
/// 全序加锁避免两个方向相反的 requeue 互相等待。
fn lock_pair(
    source: FutexKey,
    target: FutexKey,
) -> Result<
    (
        BucketGuard,
        Option<BucketGuard>,
    ),
    Status,
> {
    let (source_bucket, target_bucket) = (bucket(source), bucket(target));
    if source_bucket == target_bucket {
        return Ok((lock(source)?, None));
    }
    if source_bucket < target_bucket {
        let first = lock(source)?;
        Ok((first, Some(lock(target)?)))
    } else {
        let second = lock(target)?;
        Ok((lock(source)?, Some(second)))
    }
}

pub(super) fn requeue(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(5);
    let source = key(call, call.arg(0), options)?;
    let target = key(call, call.arg(3), options)?;
    loop {
        let (mut source_table, mut target_table) = lock_pair(source, target)?;
        let value = match read_word(call, call.arg(0))? {
            Some(value) => value,
            None => {
                drop((source_table, target_table));
                fault_in(call, call.arg(0))?;
                continue;
            }
        };
        if value != call.arg_u32(2) {
            return Err(native_abi::ERR_BAD_STATE);
        }
        queue::requeue_locked(
            &mut source_table,
            target_table.as_deref_mut(),
            &queue::Requeue {
                source,
                target,
                wake: call.arg(1),
                requeue: call.arg(4),
            },
            &mut WaiterHooks,
        );
        return Ok(());
    }
}
