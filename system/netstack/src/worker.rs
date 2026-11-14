//! 执行分区承载域协议和独立网卡队列。CPU 调度变更只在批次边界重新分配。

mod queues;
pub(crate) use queues::DeviceQueues;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::namespace::DomainScan;
use crate::time::get_time_us;
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use native_runtime::sync::{Mutex, Once};
use native_runtime::system::{deadline_after, monotonic, sleep_until};
use native_runtime::{Resource, Thread, futex, thread::JoinHandle};

// 每批已有独立包数预算。连续积压超过该时限后在批次边界让出 CPU。
const WORK_BUDGET_US: u64 = 2_000;
// 只为争用阻塞的域重试，避免同核追逐被抢占的持锁线程。
const CONTENTION_RETRY_NS: i64 = 1_000_000;
const QUEUE_WORK: u32 = 1;
const PROTOCOL_WORK: u32 = 2;
const ALL_WORK: u32 = QUEUE_WORK | PROTOCOL_WORK;
const WAITING: u32 = 4;

#[derive(Default)]
struct Wake {
    // OWNER: 分区在启动线程前固定地址。生产者合并工作位，唯一执行线程清位或宣布等待。
    // 等待位与工作位共用一个字，避免发布待办与登记 futex 之间出现遗漏。
    requested: AtomicU32,
}

impl Wake {
    fn request(&self, work: u32) {
        if self.requested.fetch_or(work, Ordering::AcqRel) == WAITING {
            futex::wake(&self.requested, 1, 0);
        }
    }

    fn pending(&self) -> u32 {
        self.requested.load(Ordering::Acquire) & ALL_WORK
    }

    fn take(&self) -> u32 {
        if self.pending() == 0 {
            0
        } else {
            self.requested.swap(0, Ordering::AcqRel) & ALL_WORK
        }
    }

    /// 返回已合并的待办位。超时返回零，取消返回 None。
    fn wait(&self, deadline: i64) -> Option<u32> {
        loop {
            if self
                .requested
                .compare_exchange(0, WAITING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Some(self.requested.swap(0, Ordering::AcqRel) & ALL_WORK);
            }
            // 工作先到时 futex 比较失败；登记先完成时首个生产者唤醒。迟到的 wake
            // 可以命中下一次等待，因此只有实际工作或到期才交还控制。
            let result = futex::wait(&self.requested, WAITING, deadline, 0);
            let work = self.requested.swap(0, Ordering::AcqRel) & ALL_WORK;
            match result {
                Ok(()) | Err(native_abi::ERR_BAD_STATE) if work != 0 => return Some(work),
                Ok(()) | Err(native_abi::ERR_BAD_STATE) => {}
                Err(native_abi::ERR_TIMED_OUT) => return Some(work),
                Err(native_abi::ERR_CANCELED) => return None,
                Err(status) => panic!("protocol wait failed: {status}"),
            }
        }
    }
}

struct Partition {
    wake: Wake,
    // OWNER: 冷路径首次发布线程；保留管理能力供独占核变更。
    thread: Mutex<Option<JoinHandle<()>>>,
    // 独占核变更须先排空每个分区的当前批次。稳态时只有该分区自己的线程取得此锁。
    batch: Mutex<()>,
    queues: queues::ReadyQueues,
}

struct Pool {
    partitions: Vec<Partition>,
    // OWNER: 独占执行线程数量，零表示全部分区共享。只有停在批次边界且通过调度准入后才发表。
    dedicated: AtomicUsize,
}

impl Pool {
    fn lanes(&self) -> usize {
        match self.dedicated.load(Ordering::Acquire) {
            0 => self.partitions.len(),
            count => count,
        }
    }
}

// OWNER: 进程的执行分区及唤醒源；槽位在启动时准备，线程按实际创建的域按需启动。
static WORKERS: Once<Pool> = Once::new();

fn pool() -> &'static Pool {
    WORKERS
        .get()
        .expect("forwarding pool started before namespace creation")
}

/// 返回整个进程生命周期内固定的执行分区数。
pub(crate) fn partition_count() -> usize {
    pool().partitions.len()
}

/// 启动网络执行。默认域的线程立即创建，其余分区在首次使用时创建。
/// # Errors
/// 分区槽分配或线程创建失败。
pub(crate) fn start(scheduler: Resource) -> Result<ForwardingWorker, Status> {
    let width = native_runtime::system::basic().cpu_count.max(1) as usize;
    let mut partitions = Vec::new();
    partitions
        .try_reserve_exact(width)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    partitions.resize_with(width, || Partition {
        wake: Wake::default(),
        thread: Mutex::new(None),
        batch: Mutex::new(()),
        queues: queues::ReadyQueues::new(),
    });
    WORKERS.call_once(|| Pool {
        partitions,
        dedicated: AtomicUsize::new(0),
    });
    ensure_partition(1)?;
    Ok(ForwardingWorker { scheduler })
}

/// 冷路径持有受限调度能力。NET 退出时内核自动归还独占 CPU。
pub(crate) struct ForwardingWorker {
    scheduler: Resource,
}

impl ForwardingWorker {
    /// 将 `mask` 中的 CPU 分配给并行转发线程。空集合恢复共享调度。
    /// # Errors
    /// 不存在的 CPU、无共享 CPU、执行体准入或分配失败。准入失败时保留旧布局。
    pub(crate) fn place(&self, mask: u64) -> Result<(), Status> {
        let pool = pool();
        let width = pool.partitions.len();
        let dedicated = mask.count_ones() as usize;
        if mask.checked_shr(width as u32).unwrap_or(0) != 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        if dedicated >= width {
            return Err(native_abi::ERR_NO_RESOURCES);
        }
        let mut threads = Vec::new();
        let mut batches = Vec::new();
        threads
            .try_reserve_exact(width)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        batches
            .try_reserve_exact(width)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        // 所有配置按固定顺序取线程槽锁。新建域只取自己的槽，不会在变更中途发布线程。
        threads.extend(
            pool.partitions
                .iter()
                .map(|partition| partition.thread.lock()),
        );
        let mut changes = Vec::new();
        changes
            .try_reserve_exact(width)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut created = 0u64;
        let result = (|| {
            let mut changes = changes;
            for (index, thread) in threads[..dedicated].iter_mut().enumerate() {
                if thread.is_none() {
                    **thread = Some(Partition::spawn(index)?);
                    created |= 1u64 << index;
                }
            }
            let mut cpus = (0..u64::BITS).filter(|cpu| mask & (1u64 << cpu) != 0);
            // 每次都提交已启动线程的完整配置。若上次发表后丢失确认，重放保存的配置也会释放多出的 CPU。
            for thread in threads.iter().filter_map(|slot| slot.as_ref()) {
                changes.push((thread.descriptor().handle(), cpus.next()));
            }
            batches.extend(
                pool.partitions
                    .iter()
                    .map(|partition| partition.batch.lock()),
            );
            Thread::set_isolation(&self.scheduler, &changes)
        })();
        if result.is_ok() {
            pool.dedicated.store(dedicated, Ordering::Release);
        }
        drop(batches);
        for (index, mut slot) in threads.into_iter().enumerate() {
            let canceled = if result.is_err() && created & (1u64 << index) != 0 {
                slot.take()
            } else {
                None
            };
            drop(slot);
            if let Some(thread) = canceled {
                // 未发布的线程只等待自己的槽锁，看到撤销后直接退出。
                thread.join();
            }
        }
        // 变更期间被消耗但未能取得批次锁的唤醒，在发布布局后重新投递。
        for partition in &pool.partitions {
            partition.wake.request(ALL_WORK);
        }
        result
    }
}

/// 从实际域 ID 导出启动后固定的共享执行分区。
pub(crate) fn partition(domain: u64) -> usize {
    ((domain - 1) % pool().partitions.len() as u64) as usize
}

/// 在域发布前准备它的执行线程。失败时调用者不能登记该域。
/// # Errors
/// 线程准备或启动失败。
pub(crate) fn ensure_partition(domain: u64) -> Result<(), Status> {
    ensure_slot(partition(domain))
}

fn ensure_slot(index: usize) -> Result<(), Status> {
    let slot = &pool().partitions[index];
    let mut thread = slot.thread.lock();
    if thread.is_none() {
        *thread = Some(Partition::spawn(index)?);
    }
    Ok(())
}

impl Partition {
    // 调用者持有槽锁，启动后的线程取得该锁才会观察发布或撤销结果。
    fn spawn(index: usize) -> Result<JoinHandle<()>, Status> {
        let thread = native_runtime::thread::prepare(b"netstack-protocol", move || {
            let current = native_runtime::thread::current().handle();
            let slot = &pool().partitions[index];
            let published = slot
                .thread
                .lock()
                .as_ref()
                .is_some_and(|worker| worker.descriptor().handle().raw() == current.raw());
            if published {
                run(index);
            }
        })?;
        thread
            .descriptor()
            .handle()
            .set_priority(device_protocol::network::FORWARDING_WORKER_PRIORITY)?;
        thread.start()
    }
}

/// 唤醒承载 `domain` 的线程。布局变更期间同时补发给新归属。
pub(crate) fn request_poll(domain: u64) {
    request_slot(partition(domain), PROTOCOL_WORK);
}

fn request_slot(index: usize, work: u32) {
    let pool = pool();
    loop {
        let lanes = pool.lanes();
        pool.partitions[index % lanes].wake.request(work);
        if lanes == pool.lanes() {
            return;
        }
    }
}

fn run(index: usize) {
    let pool = pool();
    let slot = &pool.partitions[index];
    let wake = &slot.wake;
    let mut deadline = DEADLINE_INFINITE;
    let mut pending = 0;
    let mut blocked = false;
    let mut previous = pool.partitions.len();
    let mut scan = DomainScan::new(index, previous);
    let mut queue_cursor = index;
    // 已通知或已到期的工作继续使用同一截止；只有让出 CPU 或回到空闲等待才重新计时。
    let mut work_until = 0;
    loop {
        if pending == 0 {
            let Some(requested) = wake.wait(deadline) else {
                return;
            };
            pending = if requested == 0 { PROTOCOL_WORK } else { requested };
        } else {
            pending |= wake.take();
        }
        let started = get_time_us();
        if work_until == 0 {
            work_until = started.saturating_add(WORK_BUDGET_US);
        } else if started >= work_until {
            if sleep_until(0).is_err() {
                return;
            }
            work_until = get_time_us().saturating_add(WORK_BUDGET_US);
        }
        let Some(batch) = slot.batch.try_lock() else {
            // 只有布局变更可阻止本线程取得批次锁。变更退出时重新通知两类工作。
            pending = 0;
            deadline = DEADLINE_INFINITE;
            continue;
        };
        let lanes = pool.lanes();
        if index >= lanes {
            pending = 0;
            deadline = DEADLINE_INFINITE;
            continue;
        }
        if lanes != previous {
            previous = lanes;
            scan = DomainScan::new(index, lanes);
            queue_cursor = index;
            blocked = false;
            pending |= ALL_WORK;
        }
        let mut polled_domains = false;
        let exhausted = loop {
            if pending & QUEUE_WORK != 0 {
                pending &= !QUEUE_WORK;
                pending |= queues::dispatch(index, lanes, &mut queue_cursor, work_until);
            }
            if pending & PROTOCOL_WORK != 0
                || deadline != DEADLINE_INFINITE && deadline <= monotonic()
            {
                pending &= !PROTOCOL_WORK;
                let work = crate::inet::dispatch_network_work(&mut scan, work_until);
                if work.backlog {
                    pending |= PROTOCOL_WORK;
                }
                blocked |= work.blocked;
                polled_domains = true;
                deadline = DEADLINE_INFINITE;
            }
            if get_time_us() >= work_until {
                break true;
            }
            if pending == 0 || polled_domains && pending & PROTOCOL_WORK == 0 {
                break false;
            }
            // 同一 CPU 上的协议批次可填入 TX，队列积压也可能接到新协议请求。
            // 在下一批前合并已发表的通知，不等到整个时间预算用完。
            pending |= wake.take();
        };
        drop(batch);
        if exhausted {
            if sleep_until(0).is_err() {
                return;
            }
            work_until = 0;
        }
        if polled_domains && pending & PROTOCOL_WORK == 0 {
            if wake.pending() & PROTOCOL_WORK != 0 {
                continue;
            }
            if work_until == 0 {
                work_until = get_time_us().saturating_add(WORK_BUDGET_US);
            }
            // 即使队列仍积压，也为已排空的协议工作登记实际截止，避免饿死计时器。
            let Ok((next, scan_blocked)) =
                crate::inet::network_poll_deadline(index, lanes, |earliest| {
                    let now = get_time_us();
                    if wake.pending() & PROTOCOL_WORK != 0
                        || lanes != pool.lanes()
                        || earliest <= 0
                        || now >= earliest as u64 / 1000
                    {
                        return Ok(false);
                    }
                    if now >= work_until {
                        sleep_until(0)?;
                        work_until = get_time_us().saturating_add(WORK_BUDGET_US);
                    }
                    Ok(true)
                })
            else {
                return;
            };
            deadline = if blocked || scan_blocked {
                next.min(deadline_after(CONTENTION_RETRY_NS))
            } else {
                next
            };
            blocked = false;
        }
        if pending == 0
            && wake.pending() == 0
            && (deadline == DEADLINE_INFINITE || deadline > monotonic())
        {
            work_until = 0;
        }
    }
}
