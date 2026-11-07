//! 按内存身份分桶的等待索引；私有 context 由地址空间拥有，共享 context 由调用域保活。
//! 不解释用户地址、syscall、errno 或 Linux 标志；调用者只在桶锁内用无等待读取比较值。
use super::{IrqMutex, IrqMutexGuard, TaskEvent, WaitResult};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

pub(crate) mod queue;
use queue::{FutexKey, Slot};

pub(crate) type Bucket = queue::Bucket<Arc<Waiter>>;
pub(crate) type BucketGuard<'a> = IrqMutexGuard<'a, Bucket>;

/// 一个固定桶拓扑。桶在 context 发表前一次准备，桶锁内不分配、等待或缺页。
pub(crate) struct Context {
    buckets: Box<[IrqMutex<Bucket>]>,
}
impl Context {
    /// 准备 `buckets` 个桶与唯一 context，数量必须为至少 2 的 2 次幂。
    ///
    /// # Errors
    /// 预分配或 Arc 分配失败返回 `Err(())`，不发表部分 context。
    pub(crate) fn try_new(buckets: usize) -> Result<Arc<Self>, ()> {
        assert!(buckets.is_power_of_two() && buckets >= 2);
        let mut entries = Vec::new();
        entries.try_reserve_exact(buckets).map_err(|_| ())?;
        entries.extend((0..buckets).map(|_| IrqMutex::new(Bucket::new())));
        Arc::try_new(Self {
            buckets: entries.into_boxed_slice(),
        })
        .map_err(|_| ())
    }

    /// 跨 context requeue 的全序身份：context 存活地址、桶下标；锁持有期间不能更换桶拓扑。
    pub(crate) fn lock_order(&self, key: FutexKey) -> (usize, usize) {
        (
            core::ptr::from_ref(self) as usize,
            key.bucket(self.buckets.len()),
        )
    }

    /// 锁住 key 所在桶；调用者须先准备所有可失败存储，guard 不得跨等待/用户缺页。
    pub(crate) fn lock(&self, key: FutexKey) -> BucketGuard<'_> {
        self.buckets[key.bucket(self.buckets.len())].lock()
    }
}

/// 一个等待者：当前登记位置只在其所在桶锁内修改，弱引用不与 context 的队列形成引用环。
pub(crate) struct Waiter {
    event: TaskEvent,
    // FLAG: 唤醒胜过并发超时；发布者在摘除节点后 Release，等待者在取消前后 Acquire。
    woken: AtomicBool,
    // OWNER: requeue 的唯一登记位置。取消先快照，再取该桶锁并复核；不能信任锁外快照。
    registration: Mutex<(Weak<Context>, Slot)>,
}
impl Waiter {
    /// 构造尚未入队的等待记录；context 已预分配，不在这里分配队列节点。
    pub(crate) fn new(context: &Arc<Context>, slot: Slot) -> Self {
        Self {
            event: TaskEvent::new(),
            woken: AtomicBool::new(false),
            registration: Mutex::new((Arc::downgrade(context), slot)),
        }
    }

    /// 锁外等待到 signal、打断或绝对截止时间；`None` 表示没有 deadline。
    pub(crate) fn wait(&self, deadline: Option<u64>) -> WaitResult {
        match deadline {
            Some(deadline) => self.event.wait_interruptible_until(deadline),
            None => self.event.wait_interruptible(),
        }
    }

    /// 发布唯一唤醒结果；桶已摘除节点，signal 无分配、不等待。
    pub(crate) fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        self.event.signal();
    }

    /// 唤醒是否已经战胜取消。
    pub(crate) fn is_woken(&self) -> bool {
        self.woken.load(Ordering::Acquire)
    }

    /// 取得当前登记快照；context 已退休表示它的队列也已摘除全部 owner。
    pub(crate) fn registration(&self) -> Option<(Arc<Context>, Slot)> {
        let registration = self.registration.lock();
        Some((registration.0.upgrade()?, registration.1))
    }

    /// 在已持快照桶锁后复核，封闭与 requeue 的交错。
    pub(crate) fn registered_at(&self, context: &Arc<Context>, slot: Slot) -> bool {
        let registration = self.registration.lock();
        registration.0.as_ptr() == Arc::as_ptr(context) && registration.1 == slot
    }

    /// 源/目标桶锁均持有时移交登记；不分配，也不延长 context 寿命。
    pub(crate) fn moved(&self, context: &Arc<Context>, slot: Slot) {
        *self.registration.lock() = (Arc::downgrade(context), slot);
    }
}
