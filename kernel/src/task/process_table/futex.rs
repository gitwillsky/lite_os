use super::*;
use crate::memory::{FutexKey, UserAccessError};
use crate::sync::TaskMutexWaitPreparation;

/// futex WAIT 在 task layer 的精确结果分类。
#[derive(Debug, Clone, Copy)]
pub(crate) enum FutexWaitError {
    /// WAIT value 或 CMP_REQUEUE expected 不匹配。
    Again,
    /// futex word 不可从 calling address space 读取。
    Fault,
    /// 地址未对齐、为空或 bitset 为空。
    Invalid,
    /// absolute monotonic deadline 已到期。
    TimedOut,
    /// 可投递 signal 中断了阻塞等待。
    Interrupted,
    /// wait registry 无法在 publication 前预留完整 membership。
    OutOfMemory,
    /// 原生 WAKE_OP 不支持的 operation/comparison。
    Unsupported,
}

fn pair_error(error: crate::memory::FutexAccessError) -> FutexWaitError {
    match error {
        crate::memory::FutexAccessError::Unaligned => FutexWaitError::Invalid,
        crate::memory::FutexAccessError::Memory(_error) => FutexWaitError::Fault,
    }
}

/// 按 memory-domain key 等待用户 u32 改变；先发布 Arming membership，
/// 再在 memory owner 内复查 key/value，避免 memory 与 wait shard 互相嵌套。
///
/// # Parameters
///
/// - `task`: 发起 syscall 且将被阻塞的 calling Thread owner。
/// - `address`: 4-byte aligned 用户地址。
/// - `expected`: 入队前必须匹配的当前值。
/// - `private`: true 使用 AddressSpace key；false 允许 shared backing/file key。
/// - `deadline`: 可选的绝对 monotonic 纳秒 deadline。
/// - `bitset`: waiter 的非零匹配 mask。
///
/// # Returns
///
/// 被 wake 后返回成功；值不等、fault、对齐错误、超时或 signal interruption 返回明确分类。
pub(crate) fn futex_wait(
    task: Arc<TaskControlBlock>,
    address: usize,
    expected: u32,
    private: bool,
    deadline: Option<u64>,
    bitset: u32,
) -> Result<(), FutexWaitError> {
    if address & 3 != 0 || bitset == 0 {
        return Err(FutexWaitError::Invalid);
    }
    loop {
        let key = task
            .with_futex_word(address, private, |key, current_value| {
                if current_value != expected {
                    return Err(FutexWaitError::Again);
                }
                if deadline.is_some_and(|value| value <= get_time_ns()) {
                    return Err(FutexWaitError::TimedOut);
                }
                if task.has_deliverable_signal() {
                    return Err(FutexWaitError::Interrupted);
                }
                Ok(key)
            })
            .map_err(|_| FutexWaitError::Fault)??;
        let ticket = WAIT_REGISTRY.allocate_ticket();
        let wait = ticket
            .prepare_futex(key, bitset, deadline, task.clone())
            .map_err(|()| FutexWaitError::OutOfMemory)?;
        let transaction = WAIT_REGISTRY
            .futex_transaction(&[key], None)
            .map_err(|()| FutexWaitError::OutOfMemory)?;
        // Arming source node 先于第二次 value/key 复查发布。wake 若命中只记录
        // Notified，不会唤醒仍为 Running 的 task；因此 memory callback 不再嵌套 registry lock。
        let published = WAIT_REGISTRY.publish(wait);
        let confirmed =
            match task.with_futex_word(address, private, |confirmed_key, current_value| {
                if confirmed_key != key {
                    return Ok(false);
                }
                if current_value != expected {
                    return Err(FutexWaitError::Again);
                }
                if deadline.is_some_and(|value| value <= get_time_ns()) {
                    return Err(FutexWaitError::TimedOut);
                }
                if task.has_deliverable_signal() {
                    return Err(FutexWaitError::Interrupted);
                }
                Ok(true)
            }) {
                Ok(Ok(confirmed)) => Ok(confirmed),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(FutexWaitError::Fault),
            };
        match confirmed {
            Ok(false) => match published.cancel() {
                CancelOutcome::Cancelled => continue,
                CancelOutcome::Notified(result) => return futex_notification(result),
            },
            Err(error) => match published.cancel() {
                CancelOutcome::Cancelled => return Err(error),
                CancelOutcome::Notified(result) => return futex_notification(result),
            },
            Ok(true) => {
                drop(transaction);
                let arm = match published.prepare_arm() {
                    Ok(arm) => arm,
                    Err(result) => return futex_notification(result),
                };
                let prepared =
                    super::context_switch::prepare_current_block(&task, arm, |arm, _| {
                        WaitMembership::Futex(arm.arm())
                    });
                return futex_notification(prepared.suspend());
            }
        }
    }
}

fn futex_notification(result: WaitResult) -> Result<(), FutexWaitError> {
    match result {
        WaitResult::Woken => Ok(()),
        WaitResult::TimedOut => Err(FutexWaitError::TimedOut),
        WaitResult::Interrupted => Err(FutexWaitError::Interrupted),
        WaitResult::OutOfMemory => unreachable!("published wait cannot receive OOM"),
    }
}

/// 在 memory owner 内解析稳定 key，释放 memory lock 后再唤醒最多 `count` 个 waiter。
///
/// # Parameters
///
/// - `count`: 最大唤醒数。
/// - `bitset`: wake 与 waiter mask 的非零交集条件。
/// - `preparation`: Thread 已预留的 cleanup waiter；同一次清理复用 MM 与 operation acquisition。
/// - `with_key`: 使用传入 preparation 解析稳定 key；归还 MM guard 后才取得 operation guard。
///
/// # Returns
///
/// 实际消费的 waiter 数；key 解析 fault 或非法 bitset 返回错误。
pub(in crate::task) fn futex_wake_with_key(
    count: usize,
    bitset: u32,
    preparation: &mut TaskMutexWaitPreparation,
    with_key: impl FnOnce(
        &mut TaskMutexWaitPreparation,
        &mut dyn FnMut(FutexKey),
    ) -> Result<(), UserAccessError>,
) -> Result<usize, FutexWaitError> {
    if bitset == 0 {
        return Err(FutexWaitError::Invalid);
    }
    let mut key = None;
    with_key(preparation, &mut |resolved| key = Some(resolved))
        .map_err(|_| FutexWaitError::Fault)?;
    let key = key.expect("futex key resolver did not publish an identity");
    let transaction = WAIT_REGISTRY
        .futex_transaction(&[key], Some(preparation))
        .expect("prepared mandatory futex acquisition cannot allocate");
    let mut claimed = wait_registry::ClaimedBatch::new();
    let consumed = transaction.wake(key, count, bitset, &mut claimed);
    drop(transaction);
    wake_claimed(&mut claimed);
    Ok(consumed)
}

/// 唤醒同一 memory-domain key 上最多 `count` 个匹配 waiter。
///
/// # Parameters
///
/// - `task`: 提供 futex 地址空间与 shared backing 解析上下文的 Thread owner。
/// - `address`: 4-byte aligned 用户地址。
/// - `private`: 是否强制 AddressSpace scope。
/// - `count`: 最大唤醒数。
/// - `bitset`: wake 与 waiter mask 的非零交集条件。
///
/// # Returns
///
/// 实际消费的 waiter 数。
pub(crate) fn futex_wake(
    task: &TaskControlBlock,
    address: usize,
    private: bool,
    count: usize,
    bitset: u32,
) -> Result<usize, FutexWaitError> {
    if address & 3 != 0 || bitset == 0 {
        return Err(FutexWaitError::Invalid);
    }
    let key = task
        .with_futex_key(address, private, |key| key)
        .map_err(|_| FutexWaitError::Fault)?;
    let transaction = WAIT_REGISTRY
        .futex_transaction(&[key], None)
        .map_err(|()| FutexWaitError::OutOfMemory)?;
    let mut claimed = wait_registry::ClaimedBatch::new();
    let consumed = transaction.wake(key, count, bitset, &mut claimed);
    drop(transaction);
    wake_claimed(&mut claimed);
    Ok(consumed)
}

/// 原子比较 source word，唤醒一部分 waiter，并把其余 waiter 改挂到 target key。
///
/// # Parameters
///
/// - `task`: 提供 source/target 地址空间与 shared backing 解析上下文的 Thread owner。
/// - `source`: 原 futex 地址。
/// - `target`: 目标 futex 地址。
/// - `private`: 是否强制两个地址都使用 AddressSpace scope。
/// - `wake_count`: 最大唤醒数。
/// - `requeue_count`: 最大迁移数。
/// - `compare`: `CMP_REQUEUE` 的 source expected；普通 `REQUEUE` 为 None。
///
/// # Returns
///
/// 成功返回 wake+requeue 总数；地址 fault 或 compare mismatch 返回明确分类。
pub(crate) fn futex_requeue(
    task: &TaskControlBlock,
    source: usize,
    target: usize,
    private: bool,
    wake_count: usize,
    requeue_count: usize,
    compare: Option<u32>,
) -> Result<usize, FutexWaitError> {
    if source & 3 != 0 {
        return Err(FutexWaitError::Invalid);
    }
    loop {
        let keys = task
            .with_futex_requeue(source, target, private, false, |source, target, _| {
                [source, target]
            })
            .map_err(pair_error)?;
        let transaction = WAIT_REGISTRY
            .futex_transaction(&keys, None)
            .map_err(|()| FutexWaitError::OutOfMemory)?;
        let mut waiters = wait_registry::ClaimedBatch::new();
        let result = task
            .with_futex_requeue(
                source,
                target,
                private,
                compare.is_some(),
                |source_key, target_key, current| {
                    if [source_key, target_key] != keys {
                        return Ok(None);
                    }
                    if compare.is_some_and(|expected| Some(expected) != current) {
                        return Err(FutexWaitError::Again);
                    }
                    let woken = transaction.wake(source_key, wake_count, u32::MAX, &mut waiters);
                    let moved = transaction.requeue(source_key, target_key, requeue_count);
                    Ok(Some(woken + moved))
                },
            )
            .map_err(pair_error)?;
        drop(transaction);
        wake_claimed(&mut waiters);
        if let Some(count) = result? {
            return Ok(count);
        }
    }
}

fn wake_claimed(waiters: &mut wait_registry::ClaimedBatch) {
    while let Some(entry) = waiters.pop() {
        crate::task::processor::wake_futex_task(entry.task, entry.id, WaitResult::Woken);
    }
}

/// 在同一个实际 source transaction 内更新 target，并按旧值比较唤醒两个队列。
/// 不支持的 comparison 在更新之后返回 ENOSYS；任一 fault 不消费 waiter。
pub(crate) fn futex_wake_op(
    task: &TaskControlBlock,
    source: usize,
    target: usize,
    private: bool,
    counts: [usize; 2],
    update: Option<crate::memory::UserAtomicU32Op>,
    compare: impl FnOnce(u32) -> Result<bool, FutexWaitError>,
) -> Result<usize, FutexWaitError> {
    if source & 3 != 0 {
        return Err(FutexWaitError::Invalid);
    }
    let limits = task.user_fault_limits();
    loop {
        let keys = task
            .with_futex_wake_op(source, target, private, |source, target, _| {
                [source, target]
            })
            .map_err(pair_error)?;
        let transaction = WAIT_REGISTRY
            .futex_transaction(&keys, None)
            .map_err(|()| FutexWaitError::OutOfMemory)?;
        let mut claimed = wait_registry::ClaimedBatch::new();
        // FnOnce comparison 只在 key 已复查、actual RMW 已成功后消费。
        let result = task
            .with_futex_wake_op(source, target, private, |source_key, target_key, memory| {
                if [source_key, target_key] != keys {
                    return Ok(None);
                }
                let update = update.ok_or(FutexWaitError::Unsupported)?;
                let word: crate::memory::UserAtomicU32<'_> = memory
                    .futex_atomic_word(target, limits)
                    .map_err(|_| FutexWaitError::Fault)?;
                let old = word.update(update);
                Ok(Some(old))
            })
            .map_err(pair_error)??;
        let Some(old) = result else {
            drop(transaction);
            continue;
        };
        let second = compare(old)?;
        let mut count = transaction.wake(keys[0], counts[0], u32::MAX, &mut claimed);
        if second {
            count += transaction.wake(keys[1], counts[1], u32::MAX, &mut claimed);
        }
        drop(transaction);
        wake_claimed(&mut claimed);
        return Ok(count);
    }
}
