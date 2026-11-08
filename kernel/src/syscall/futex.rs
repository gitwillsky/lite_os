//! native futex 参数、地址身份与用户值比较；等待索引属于 execution::sync::futex。
//!
//! 私有 key 使用实际地址空间的 context，共享 key 使用全局 context。空间锁先于桶锁取得，
//! 先在桶锁外稳定用户映射，再在桶锁内原子比较和登记；不会在 IRQ-safe 桶锁内睡眠或分配。
//! requeue 按 (context 身份, 桶下标) 全序加锁；超时取消在快照对应的桶内复核当前位置。
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use native_abi::Status;
use spin::Once;

use super::Call;
use crate::execution::sync::{
    LocalIrqGuard, WaitResult,
    futex::{Bucket, BucketGuard, Context, Waiter, queue},
};
use queue::{FutexKey, Slot};

// OWNER: 仅共享 VMO key 的全局索引；私有 futex 由 AddressSpace 唯一拥有，互不争用桶。
// 桶数在首次共享调用时固定，此初始化只分配、不等待，之后不更换拓扑，否则相同 key 会丢失唤醒。
static SHARED: Once<Arc<Context>> = Once::new();
// OWNER: 唯一登记序号，Relaxed 只提供 FIFO 顺序，不发表等待结果。
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

fn shared_context() -> Result<Arc<Context>, Status> {
    SHARED
        .try_call_once(|| {
            let buckets = (256 * crate::execution::cpu::count()).next_power_of_two();
            Context::try_new(buckets)
        })
        .cloned()
        .map_err(|_| native_abi::ERR_NO_MEMORY)
}

fn location(
    call: &Call<'_>,
    address: usize,
    options: u32,
) -> Result<(Arc<Context>, FutexKey), Status> {
    if !address.is_multiple_of(4) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let space = call
        .thread
        .address_space()
        .ok_or(native_abi::ERR_BAD_STATE)?;
    match options {
        0 => Ok((
            space.futex_context(address).map_err(|error| match error {
                crate::vm::VmError::NotMapped => native_abi::ERR_INVALID_POINTER,
                error => crate::object::vm_status(error),
            })?,
            FutexKey::Private {
                space: space.futex_owner(address),
                address,
            },
        )),
        native_abi::futex::SHARED => {
            let (vmo, offset) = space
                .resolve_object(address)
                .map_err(|_| native_abi::ERR_INVALID_POINTER)?;
            Ok((shared_context()?, FutexKey::Shared { vmo, offset }))
        }
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}

/// 可阻塞的映射稳定先于桶事务；闭包只在拿到桶锁后读取，封闭值改变/唤醒与入队的窗口。
fn with_word<R>(
    call: &Call<'_>,
    address: usize,
    operation: impl FnOnce(&core::sync::atomic::AtomicU32) -> R,
) -> Result<R, Status> {
    call.thread
        .address_space()
        .ok_or(native_abi::ERR_BAD_STATE)?
        .with_resident_word(address, operation)
        .map_err(|_| native_abi::ERR_INVALID_POINTER)
}

pub(super) fn wait(call: &Call<'_>) -> Result<(), Status> {
    let address = call.arg(0);
    let (context, key) = location(call, address, call.arg_u32(3))?;
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    let waiter = Arc::try_new(Waiter::new(&context, (key, ticket)))
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    // 唯一可失败的节点准备在 IRQ-safe guard 之外；requeue 转移这个节点，不重新分配。
    let entry =
        Bucket::prepare((key, ticket), waiter.clone()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    with_word(call, address, |word| {
        let mut table = context.lock(key);
        if word.load(Ordering::Acquire) != call.arg_u32(1) {
            return Err(native_abi::ERR_BAD_STATE);
        }
        table.enqueue(entry);
        Ok(())
    })??;
    let result = waiter.wait(super::deadline(call.arg(2)));
    if waiter.is_woken() {
        return Ok(());
    }
    remove(&waiter);
    if waiter.is_woken() {
        return Ok(());
    }
    Err(match result {
        WaitResult::TimedOut => native_abi::ERR_TIMED_OUT,
        WaitResult::OutOfMemory => native_abi::ERR_NO_MEMORY,
        WaitResult::Woken | WaitResult::Interrupted => native_abi::ERR_CANCELED,
    })
}

fn remove(waiter: &Arc<Waiter>) {
    while let Some((context, slot)) = waiter.registration() {
        let mut table = context.lock(slot.0);
        if !waiter.registered_at(&context, slot) {
            continue;
        }
        drop(table.remove(&slot));
        return;
    }
}

fn wake_waiter(waiter: Arc<Waiter>) {
    waiter.wake();
}

struct WaiterHooks<'a>(&'a Arc<Context>);
impl queue::RequeueHooks<Arc<Waiter>> for WaiterHooks<'_> {
    fn wake(&mut self, waiter: Arc<Waiter>) {
        waiter.wake();
    }
    fn next_ticket(&mut self) -> u64 {
        NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
    }
    fn moved(&mut self, waiter: &Arc<Waiter>, position: Slot) {
        waiter.moved(self.0, position);
    }
}

pub(super) fn wake(call: &Call<'_>) -> Result<(), Status> {
    let (context, key) = location(call, call.arg(0), call.arg_u32(2))?;
    let mut table = context.lock(key);
    let woken = queue::wake_locked(&mut table, key, call.arg(1), wake_waiter);
    drop(table);
    call.write_optional(call.arg(3), &woken)
}

/// 一次双桶锁事务。外层 IRQ guard 最后析构，内层两把锁都观察到 IRQ 已关闭，
/// 因而不论 source/target 字段的析构顺序如何，都不会在另一桶尚未解锁时提前开 IRQ。
struct LockedBuckets<'a> {
    source: BucketGuard<'a>,
    target: Option<BucketGuard<'a>>,
    _irq: LocalIrqGuard,
}

fn lock_pair<'a>(
    source: &'a Context,
    source_key: FutexKey,
    target: &'a Context,
    target_key: FutexKey,
) -> LockedBuckets<'a> {
    let irq = LocalIrqGuard::disable();
    let (source_order, target_order) =
        (source.lock_order(source_key), target.lock_order(target_key));
    let (source, target) = if source_order == target_order {
        (source.lock(source_key), None)
    } else if source_order < target_order {
        let first = source.lock(source_key);
        (first, Some(target.lock(target_key)))
    } else {
        let second = target.lock(target_key);
        (source.lock(source_key), Some(second))
    };
    LockedBuckets {
        source,
        target,
        _irq: irq,
    }
}

pub(super) fn requeue(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(5);
    let (source_context, source) = location(call, call.arg(0), options)?;
    let (target_context, target) = location(call, call.arg(3), options)?;
    with_word(call, call.arg(0), |word| {
        let mut buckets = lock_pair(&source_context, source, &target_context, target);
        if word.load(Ordering::Acquire) != call.arg_u32(2) {
            return Err(native_abi::ERR_BAD_STATE);
        }
        queue::requeue_locked(
            &mut buckets.source,
            buckets.target.as_deref_mut(),
            &queue::Requeue {
                source,
                target,
                wake: call.arg(1),
                requeue: call.arg(4),
            },
            &mut WaiterHooks(&target_context),
        );
        Ok(())
    })?
}
