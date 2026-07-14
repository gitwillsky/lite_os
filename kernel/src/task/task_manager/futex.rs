use super::*;

/// @description futex WAIT 在 task layer 的精确结果分类。
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
}

/// @description 按 memory-domain key 等待用户 u32 改变，队列锁覆盖 key/value 解析与发布。
///
/// @param task 发起 syscall 且将被阻塞的 calling Thread owner。
/// @param address 4-byte aligned 用户地址。
/// @param expected 入队前必须匹配的当前值。
/// @param private true 使用 AddressSpace key；false 允许 shared backing/file key。
/// @param deadline 可选的绝对 monotonic 纳秒 deadline。
/// @param bitset waiter 的非零匹配 mask。
/// @return 被 wake 后返回成功；值不等、fault、对齐错误、超时或 signal interruption 返回明确分类。
pub(crate) fn futex_wait(
    task: Arc<TaskControlBlock>,
    address: usize,
    expected: u32,
    private: bool,
    deadline: Option<u64>,
    bitset: u32,
) -> Result<(), FutexWaitError> {
    if address == 0 || address & 3 != 0 || bitset == 0 {
        return Err(FutexWaitError::Invalid);
    }
    let queue = INDEXED_WAIT_QUEUE.lock();
    let prepared = task
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

            Ok(super::context_switch::prepare_current_block(
                &task,
                queue,
                |queue, current| {
                    let wait_id = queue.insert_futex(key, bitset, deadline, current);
                    WaitMembership::Futex(wait_id)
                },
            ))
        })
        .map_err(|_| FutexWaitError::Fault)??;
    match prepared.suspend() {
        WaitResult::Woken => Ok(()),
        WaitResult::TimedOut => Err(FutexWaitError::TimedOut),
        WaitResult::Interrupted => Err(FutexWaitError::Interrupted),
    }
}

/// @description 唤醒同一 memory-domain key 上最多 `count` 个匹配 waiter。
///
/// @param task 提供 futex 地址空间与 shared backing 解析上下文的 Thread owner。
/// @param address 4-byte aligned 用户地址。
/// @param private 是否强制 AddressSpace scope。
/// @param count 最大唤醒数。
/// @param bitset wake 与 waiter mask 的非零交集条件。
/// @return 实际消费的 waiter 数。
pub(crate) fn futex_wake(
    task: &TaskControlBlock,
    address: usize,
    private: bool,
    count: usize,
    bitset: u32,
) -> Result<usize, FutexWaitError> {
    if address == 0 || address & 3 != 0 || bitset == 0 {
        return Err(FutexWaitError::Invalid);
    }
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    let waiters = task
        .with_futex_key(address, private, |key| {
            let mut waiters = alloc::vec::Vec::new();
            for _ in 0..count {
                let Some(waiter) = queue.take_futex(key, bitset) else {
                    break;
                };
                waiters.push(waiter);
            }
            waiters
        })
        .map_err(|_| FutexWaitError::Fault)?;
    drop(queue);
    let count = waiters.len();
    for (wait_id, task) in waiters {
        crate::task::processor::wake_futex_task(task, wait_id, WaitResult::Woken);
    }
    Ok(count)
}

/// @description 原子比较 source word，唤醒一部分 waiter，并把其余 waiter 改挂到 target key。
///
/// @param task 提供 source/target 地址空间与 shared backing 解析上下文的 Thread owner。
/// @param source 原 futex 地址。
/// @param target 目标 futex 地址。
/// @param private 是否强制两个地址都使用 AddressSpace scope。
/// @param wake_count 最大唤醒数。
/// @param requeue_count 最大迁移数。
/// @param compare `CMP_REQUEUE` 的 source expected；普通 `REQUEUE` 为 None。
/// @return 成功返回 wake+requeue 总数；地址 fault 或 compare mismatch 返回明确分类。
pub(crate) fn futex_requeue(
    task: &TaskControlBlock,
    source: usize,
    target: usize,
    private: bool,
    wake_count: usize,
    requeue_count: usize,
    compare: Option<u32>,
) -> Result<usize, FutexWaitError> {
    if source == 0 || source & 3 != 0 || target == 0 || target & 3 != 0 {
        return Err(FutexWaitError::Invalid);
    }
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    let (waiters, moved) = task
        .with_futex_requeue(
            source,
            target,
            private,
            |source_key, target_key, current| {
                if compare.is_some_and(|expected| expected != current) {
                    return Err(FutexWaitError::Again);
                }
                let mut waiters = alloc::vec::Vec::new();
                for _ in 0..wake_count {
                    let Some(waiter) = queue.take_futex(source_key, u32::MAX) else {
                        break;
                    };
                    waiters.push(waiter);
                }
                let moved = queue.requeue_futex(source_key, target_key, requeue_count);
                Ok((waiters, moved))
            },
        )
        .map_err(|_| FutexWaitError::Fault)??;
    drop(queue);
    let completed = waiters.len() + moved;
    for (wait_id, task) in waiters {
        crate::task::processor::wake_futex_task(task, wait_id, WaitResult::Woken);
    }
    Ok(completed)
}
