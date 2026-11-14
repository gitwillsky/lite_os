//! 每个活动网卡队列只登记一次收发工作，域协议在实际队列推进后收到通知。

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use device_protocol::network::{RemoteNetwork, encode_error};
use native_runtime::{
    native_abi::{self, Status},
    sync::Mutex,
};

use super::{PROTOCOL_WORK, QUEUE_WORK, ensure_slot, partition, pool, request_poll, request_slot};

const JOBS_PER_ROUND: usize = 4;
const COMPLETIONS_PER_JOB: usize = 16;

struct Job {
    device: Weak<RemoteNetwork>,
    scope: Weak<crate::namespace::Namespace>,
    domain: u64,
    slot: usize,
    queue: u16,
    // 接口安装成功时由租约一次发布。零表示接口还没有身份，不能预判依赖入接口的规则。
    ingress: AtomicU32,
    // OWNER: 注册/退休在 slot 队列锁内发布，已被领取的任务在访问设备前检查。
    live: AtomicBool,
    // OWNER: slot 队列锁内维护唯一待处理成员资格；不重复排队，也不在事件回调中分配。
    queued: AtomicBool,
}

struct Ready {
    jobs: VecDeque<Arc<Job>>,
    // 每个登记在创建回调前预留一个 FIFO 位置，即使事件和回收同时到达也不分配。
    registered: usize,
}

pub(super) struct ReadyQueues {
    state: Mutex<Ready>,
    // OWNER: 在队列锁内发布是否为空，让没有设备任务的协议轮次跳过此锁。
    pending: AtomicBool,
}

impl ReadyQueues {
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(Ready {
                jobs: VecDeque::new(),
                registered: 0,
            }),
            pending: AtomicBool::new(false),
        }
    }

    fn register(&self, job: &Job) -> Result<(), Status> {
        let mut state = self.state.lock();
        let registered = state
            .registered
            .checked_add(1)
            .ok_or(native_abi::ERR_NO_MEMORY)?;
        if state.jobs.capacity() < registered {
            let additional = registered - state.jobs.len();
            state
                .jobs
                .try_reserve_exact(additional)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        }
        state.registered = registered;
        job.live.store(true, Ordering::Release);
        Ok(())
    }

    fn registration_count(&self) -> usize {
        self.state.lock().registered
    }

    fn enqueue(&self, job: &Arc<Job>) -> bool {
        let mut state = self.state.lock();
        if !job.live.load(Ordering::Relaxed) || job.queued.load(Ordering::Relaxed) {
            return false;
        }
        assert!(state.jobs.len() < state.registered && state.registered <= state.jobs.capacity());
        job.queued.store(true, Ordering::Relaxed);
        state.jobs.push_back(job.clone());
        self.pending.store(true, Ordering::Release);
        true
    }

    fn take(&self) -> Option<Arc<Job>> {
        if !self.pending.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.state.lock();
        let job = state.jobs.pop_front()?;
        job.queued.store(false, Ordering::Relaxed);
        if state.jobs.is_empty() {
            self.pending.store(false, Ordering::Release);
        }
        Some(job)
    }

    fn retire(&self, job: &Arc<Job>) {
        let mut state = self.state.lock();
        job.live.store(false, Ordering::Release);
        if job.queued.swap(false, Ordering::Relaxed) {
            state.jobs.retain(|queued| !Arc::ptr_eq(queued, job));
        }
        state.registered -= 1;
        if state.jobs.is_empty() {
            self.pending.store(false, Ordering::Release);
        }
    }
}

struct Registration(Arc<Job>);

impl Job {
    fn notify(self: &Arc<Self>) {
        if !self.live.load(Ordering::Acquire) {
            return;
        }
        let Some(device) = self.device.upgrade() else {
            return;
        };
        if (device.has_receive_work(self.queue) || device.has_transmit_work(self.queue))
            && pool().partitions[self.slot].queues.enqueue(self)
        {
            request_slot(self.slot, QUEUE_WORK);
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        pool().partitions[self.0.slot].queues.retire(&self.0);
    }
}

/// 接口租约拥有队列登记；设备和正在执行的工作不延长登记寿命。
#[derive(Default)]
pub(crate) struct DeviceQueues {
    registrations: Vec<Registration>,
}

impl DeviceQueues {
    /// 为 `device` 的所有队列准备实际工作线程并绑定活动通知。
    /// # Errors
    /// 工作槽、线程、中断路由或回调准备失败，已准备的登记随临时 owner 自动退休。
    pub(crate) fn bind(
        scope: &Arc<crate::namespace::Namespace>,
        device: &Arc<RemoteNetwork>,
    ) -> Result<Self, Status> {
        use device_model::network::NetworkDevice;

        let mut registrations = Vec::new();
        registrations
            .try_reserve_exact(usize::from(device.queue_count()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let pool = pool();
        let lanes = pool.lanes();
        let width = pool.partitions.len();
        let home = partition(scope.id());
        // 安装入口串行登记。这里的负载只用于本次分配，队列退休可并发降低登记量。
        let mut loads = Vec::new();
        loads
            .try_reserve_exact(width)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        loads.resize_with(width, PartitionLoad::default);
        for (slot, partition) in pool.partitions.iter().enumerate() {
            let registered = partition.queues.registration_count();
            loads[slot].registered = registered;
            loads[slot % lanes].lane_registered += registered;
        }
        let steer_interrupts = device.supports_queue_affinity();
        for queue in 0..device.queue_count() {
            let slot = (0..width)
                .map(|offset| (home + offset) % width)
                .min_by_key(|&slot| loads[slot].placement_key(&loads[slot % lanes]))
                .expect("network pool has at least one partition");
            // 使用固定分区编号分散 MSI-X。独占核设置只变更执行线程归属，网卡无需重新开队列或散列。
            if steer_interrupts {
                device
                    .set_queue_affinity(queue, slot as u32)
                    .map_err(encode_error)?;
            }
            registrations.push(bind_queue(scope, slot, queue, device)?);
            loads[slot].assigned += 1;
            loads[slot].registered += 1;
            loads[slot % lanes].lane_assigned += 1;
            loads[slot % lanes].lane_registered += 1;
        }
        Ok(Self { registrations })
    }

    /// 接口提交时发布每条队列所属的实际入接口序号。队列每批只读一次。
    pub(crate) fn publish_ingress(&self, ingress: u32) {
        for registration in &self.registrations {
            registration.0.ingress.store(ingress, Ordering::Release);
        }
    }

    /// 以实际登记判断发送队列能否与域协议并行。切换专用核后使用已发表的线程布局。
    pub(crate) fn separate_transmit_worker(&self, queue: u16) -> bool {
        self.registrations
            .get(usize::from(queue))
            .is_some_and(|registration| {
                let job = &registration.0;
                let lanes = pool().lanes();
                job.slot % lanes != partition(job.domain) % lanes
            })
    }
}

#[derive(Default)]
struct PartitionLoad {
    assigned: usize,
    lane_assigned: usize,
    registered: usize,
    lane_registered: usize,
}

impl PartitionLoad {
    // 先分散同一网卡的队列，保留布局扩展后的并行性。再按实际执行线程和固定分区负载分散网卡。
    fn placement_key(&self, lane: &Self) -> (usize, usize, usize, usize) {
        (
            self.assigned,
            lane.lane_assigned,
            lane.lane_registered,
            self.registered,
        )
    }
}

fn bind_queue(
    scope: &Arc<crate::namespace::Namespace>,
    slot: usize,
    queue: u16,
    device: &Arc<RemoteNetwork>,
) -> Result<Registration, Status> {
    let domain = scope.id();
    ensure_slot(slot)?;
    let job = Arc::try_new(Job {
        device: Arc::downgrade(device),
        scope: Arc::downgrade(scope),
        domain,
        slot,
        queue,
        ingress: AtomicU32::new(0),
        live: AtomicBool::new(false),
        queued: AtomicBool::new(false),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    pool().partitions[slot].queues.register(&job)?;
    let registration = Registration(job);
    let weak = Arc::downgrade(&registration.0);
    let activity: device_model::Activity = Arc::try_new(move || {
        if let Some(job) = weak.upgrade() {
            job.notify();
        }
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    device_model::network::NetworkDevice::bind_activity(device.as_ref(), queue, activity);
    Ok(registration)
}

/// 按当前布局轮转各固定槽。每轮最多领取和归还各四批 RX、发表四批 TX 并回收 64 项 TX。
/// 返回本线程的后续工作位。所有环的推进都不持域协议锁。
pub(super) fn dispatch(lane: usize, lanes: usize, next: &mut usize, work_until: u64) -> u32 {
    let partitions = &pool().partitions;
    let slots = (partitions.len() - 1 - lane) / lanes + 1;
    let mut processed = 0;
    let mut pending = 0;
    for visited in 0..slots {
        let slot = *next;
        *next = if slot + lanes < partitions.len() {
            slot + lanes
        } else {
            lane
        };
        let queue = &partitions[slot].queues;
        if let Some(job) = queue.take() {
            processed += 1;
            if job.live.load(Ordering::Acquire)
                && let Some(device) = job.device.upgrade()
            {
                // RX 与 TX 通知可合并。即使 RX 批还在等域消费，也要推进本队列 CQ。
                let mut wake_domain = false;
                let mut more = false;
                let separate_worker = partition(job.domain) % lanes != lane;
                if device.has_receive_work(job.queue) {
                    let scope = job.scope.upgrade();
                    let ingress = job.ingress.load(Ordering::Acquire);
                    let mut snapshot = None;
                    match device.advance_receive(job.queue, |packet| {
                        snapshot
                            .get_or_insert_with(|| scope.as_ref()?.protocol()?.receive_snapshot())
                            .as_ref()
                            .map_or_else(Default::default, |snapshot| {
                                snapshot.prepare(packet, separate_worker, ingress)
                            })
                    }) {
                        Ok((ready, backlog)) => {
                            wake_domain = ready;
                            more = backlog;
                        }
                        Err(_) => wake_domain = true,
                    }
                }
                if device.has_transmit_work(job.queue) {
                    match device.advance_transmit(job.queue, COMPLETIONS_PER_JOB) {
                        Ok(done) => {
                            wake_domain |= done.transmit_became_available;
                            more |= done.backlog;
                        }
                        Err(_) => wake_domain = true,
                    }
                }
                if more {
                    queue.enqueue(&job);
                }
                if wake_domain {
                    if separate_worker {
                        request_poll(job.domain);
                    } else {
                        pending |= PROTOCOL_WORK;
                    }
                }
            }
            if queue.pending.load(Ordering::Acquire) {
                pending |= QUEUE_WORK;
            }
            if processed == JOBS_PER_ROUND {
                return pending | QUEUE_WORK;
            }
            if visited + 1 < slots && crate::time::get_time_us() >= work_until {
                // 留住下一个固定槽。时间耗尽不能误认为剩余槽都没有待办。
                return pending | QUEUE_WORK;
            }
        }
    }
    pending
}
