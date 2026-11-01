//! 驱动原语：resource、中断、IOMMU、BTI 与固定内存令牌（PMT）。
//!
//! # 权限
//!
//! resource 是硬件访问的唯一授权来源：根 resource 只交给根服务，子 resource 只能是父区间的子集。
//! MMIO resource 不得覆盖 RAM，否则持有者可以映射任意内核内存。
//!
//! # 中断
//!
//! 1. 硬中断上下文只记录时间戳并把对象放入预留的待处理队列，发布一个 deferred vector；
//! 2. deferred work 唤醒等待者或向绑定的 port 投递 `packet::INTERRUPT`；
//! 3. 电平触发源在 handler 前由控制器屏蔽，`ack` 时解除，避免用户态处理前反复重投。
//!
//! # DMA
//!
//! BTI 代表一个 DMA 发起者；`pin` 固定 VMO 页并经所属 IOMMU 建立设备地址，PMT 析构时先撤销设备
//! 映射再解除固定，设备不可能访问已释放的页。

use crate::iommu::IommuBackend;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use native_abi::{ObjectType, PortPacket, Status, rights, signals};
use spin::{Mutex, Once};

use super::{KernelObject, Port, SignalState, WAITABLE_RIGHTS, next_koid, vm_status};
use crate::execution::deferred::{self, DeferredWork};
use crate::execution::interrupt::{InterruptError, InterruptHandler, InterruptVector};
use crate::execution::sync::{IrqMutex, WaitQueue, WaitResult};
use crate::vm::{PinnedRange, Vmo};

// ---------------------------------------------------------------------------------------------
// Resource
// ---------------------------------------------------------------------------------------------

mod authorization;
pub(crate) use authorization::Resource;

// ---------------------------------------------------------------------------------------------
// Interrupt
// ---------------------------------------------------------------------------------------------

struct InterruptShared {
    vector: InterruptVector,
    level: bool,
    // OWNER: 最近一次触发的单调时间戳；0 表示没有未消费的触发。
    triggered: AtomicI64,
    // FLAG: 已在待处理队列中；避免同一对象重复入队超出预留容量。
    queued: AtomicBool,
    // FLAG: 对象已销毁；等待者以 `ERR_CANCELED` 结束。
    destroyed: AtomicBool,
    waiters: WaitQueue,
    port: Mutex<Option<(Arc<Port>, u64)>>,
}

/// 交给中断控制器的绑定：持有共享状态的强引用，硬中断入队不需要查找。
struct InterruptBinding(Arc<InterruptShared>);

impl InterruptHandler for InterruptBinding {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let shared = &self.0;
        let now = crate::execution::timer::get_time_ns() as i64;
        shared.triggered.store(now.max(1), Ordering::Release);
        if !shared.queued.swap(true, Ordering::AcqRel) {
            // 绑定时已为本对象预留一格，入队不分配。
            let mut pending = PENDING.lock();
            debug_assert!(pending.queue.len() < pending.queue.capacity());
            pending.queue.push_back(shared.clone());
            drop(pending);
            deferred::raise(*PENDING_WORK.wait());
        }
        Ok(())
    }

    fn masks_until_ack(&self) -> bool {
        self.0.level
    }
}

struct PendingQueue {
    queue: VecDeque<Arc<InterruptShared>>,
    // OWNER: 存活中断对象数；容量始终不少于它，每个对象至多同时入队一次，所以硬中断入队不会扩容。
    reserved: usize,
}

// OWNER: 已触发、等待 deferred work 处理的中断；容量在绑定时为每个对象预留一格，硬中断入队不分配。
static PENDING: IrqMutex<PendingQueue> = IrqMutex::new(PendingQueue {
    queue: VecDeque::new(),
    reserved: 0,
});
// OWNER: 中断对象共用的 deferred vector；首次创建中断对象时注册。
static PENDING_WORK: Once<DeferredWork> = Once::new();

fn process_pending(_now: u64) -> bool {
    loop {
        let Some(shared) = PENDING.lock().queue.pop_front() else {
            return false;
        };
        shared.queued.store(false, Ordering::Release);
        let port = shared.port.lock().clone();
        match port {
            Some((port, key)) => {
                let timestamp = shared.triggered.swap(0, Ordering::AcqRel);
                if timestamp != 0 {
                    let _ = port.queue(PortPacket {
                        key,
                        kind: native_abi::packet::INTERRUPT,
                        status: native_abi::OK,
                        trigger: 0,
                        observed: 0,
                        payload: timestamp as u64,
                    });
                }
            }
            None => shared.waiters.wake_all(),
        }
    }
}

/// 中断对象。
pub(crate) struct Interrupt {
    koid: u64,
    shared: Arc<InterruptShared>,
    signals: SignalState,
}

impl Interrupt {
    /// 绑定中断向量。
    ///
    /// # Errors
    /// 向量已被占用或非法返回 `ERR_ALREADY_BOUND`/`ERR_INVALID_ARGS`；分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn create(vector: u32, level: bool) -> Result<Self, Status> {
        PENDING_WORK.call_once(|| {
            deferred::register(process_pending).expect("interrupt deferred vector exhausted")
        });
        let shared = Arc::try_new(InterruptShared {
            vector,
            level,
            triggered: AtomicI64::new(0),
            queued: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            port: Mutex::new(None),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let binding = Arc::try_new(InterruptBinding(shared.clone()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        // 预留在锁外分配：先扩出新存储，再在锁内交换，硬中断路径永不分配。
        let target = PENDING.lock().reserved + 1;
        let mut replacement = VecDeque::new();
        replacement
            .try_reserve_exact(target)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        {
            let mut pending = PENDING.lock();
            if pending.queue.capacity() < pending.reserved + 1 {
                replacement.append(&mut pending.queue);
                core::mem::swap(&mut pending.queue, &mut replacement);
            }
            pending.reserved += 1;
        }
        drop(replacement);
        if let Err(error) = crate::platform::bind_interrupt(vector, binding) {
            PENDING.lock().reserved -= 1;
            return Err(match error {
                InterruptError::NoMemory => native_abi::ERR_NO_MEMORY,
                _ => native_abi::ERR_ALREADY_BOUND,
            });
        }
        Ok(Self {
            koid: next_koid(),
            shared,
            signals: SignalState::new(0),
        })
    }

    /// 等待触发；返回触发时的单调时间戳。
    ///
    /// # Errors
    /// `ERR_TIMED_OUT`、`ERR_CANCELED`（对象销毁或线程终止）、`ERR_BAD_STATE`（已绑定 port）。
    pub(crate) fn wait(&self, deadline_ns: Option<u64>) -> Result<i64, Status> {
        if self.shared.port.lock().is_some() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let shared = &self.shared;
        let result = shared.waiters.wait_until(
            || {
                shared.destroyed.load(Ordering::Acquire)
                    || shared.triggered.load(Ordering::Acquire) != 0
            },
            deadline_ns,
        );
        match result {
            WaitResult::Woken if shared.destroyed.load(Ordering::Acquire) => {
                Err(native_abi::ERR_CANCELED)
            }
            WaitResult::Woken => Ok(shared.triggered.load(Ordering::Acquire)),
            WaitResult::TimedOut => Err(native_abi::ERR_TIMED_OUT),
            WaitResult::Interrupted => Err(native_abi::ERR_CANCELED),
            WaitResult::OutOfMemory => Err(native_abi::ERR_NO_MEMORY),
        }
    }

    /// 确认触发并重新允许（电平触发源解除屏蔽）。
    ///
    /// # Errors
    /// 控制器拒绝时返回 `ERR_INTERNAL`。
    pub(crate) fn ack(&self) -> Result<(), Status> {
        self.shared.triggered.store(0, Ordering::Release);
        if self.shared.level {
            crate::platform::unmask_interrupt(self.shared.vector)
                .map_err(|_| native_abi::ERR_INTERNAL)?;
        }
        Ok(())
    }

    /// 绑定 port：之后每次触发投递一个 packet，`wait` 不再可用。
    ///
    /// # Errors
    /// 已绑定返回 `ERR_ALREADY_BOUND`。
    pub(crate) fn bind_port(&self, port: Arc<Port>, key: u64) -> Result<(), Status> {
        let mut slot = self.shared.port.lock();
        if slot.is_some() {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        *slot = Some((port, key));
        Ok(())
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        let handler = crate::platform::unbind_interrupt(self.shared.vector);
        drop(handler);
        PENDING.lock().reserved -= 1;
        self.shared.destroyed.store(true, Ordering::Release);
        self.shared.waiters.wake_all();
        self.signals.update(0, signals::CANCELED);
    }
}

impl KernelObject for Interrupt {
    fn object_type(&self) -> ObjectType {
        ObjectType::Interrupt
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        WAITABLE_RIGHTS | rights::READ | rights::WRITE
    }
    fn signals(&self) -> Option<&SignalState> {
        Some(&self.signals)
    }
}

// ---------------------------------------------------------------------------------------------
// IOMMU / BTI / PMT
// ---------------------------------------------------------------------------------------------

/// IOMMU 对象。
pub(crate) struct Iommu {
    koid: u64,
    backend: Arc<dyn IommuBackend>,
}

impl Iommu {
    /// 创建 IOMMU 对象。
    ///
    /// # Errors
    /// 类型未知或硬件初始化失败。
    pub(crate) fn create(kind: u32, descriptor: &[u8]) -> Result<Self, Status> {
        if !matches!(
            kind,
            native_abi::iommu::IDENTITY | native_abi::iommu::ARM_SMMU_V3 | native_abi::iommu::RISCV
        ) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let backend = crate::iommu::create(kind, descriptor)?;
        Ok(Self {
            koid: next_koid(),
            backend,
        })
    }
}

impl KernelObject for Iommu {
    fn object_type(&self) -> ObjectType {
        ObjectType::Iommu
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE | rights::TRANSFER | rights::INSPECT
    }
}

/// DMA 发起者。
pub(crate) struct Bti {
    koid: u64,
    backend: Arc<dyn IommuBackend>,
    stream: u32,
}

impl Bti {
    /// 为 `stream` 创建 BTI。
    pub(crate) fn create(iommu: &Iommu, stream: u32) -> Self {
        Self {
            koid: next_koid(),
            backend: iommu.backend.clone(),
            stream,
        }
    }

    /// 固定 VMO 区间并建立设备地址。
    ///
    /// # Errors
    /// 参数非法、越界、pager 失败或设备地址空间耗尽。
    pub(crate) fn pin(
        self: &Arc<Self>,
        vmo: &Arc<Vmo>,
        offset: u64,
        length: u64,
        writable: bool,
    ) -> Result<Pmt, Status> {
        let pinned = vmo.pin(offset, length).map_err(vm_status)?;
        let mut addresses = Vec::new();
        self.backend
            .map(self.stream, pinned.pages(), writable, &mut addresses)?;
        Ok(Pmt {
            koid: next_koid(),
            bti: self.clone(),
            addresses,
            _pinned: pinned,
        })
    }
}

impl KernelObject for Bti {
    fn object_type(&self) -> ObjectType {
        ObjectType::Bti
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE | rights::TRANSFER | rights::READ | rights::WRITE | rights::INSPECT
    }
}

/// 固定内存令牌：析构时先撤销设备映射，再解除固定。
pub(crate) struct Pmt {
    koid: u64,
    bti: Arc<Bti>,
    addresses: Vec<u64>,
    // OWNER: 字段在 Drop 之后析构：设备映射撤销完成前页保持固定。
    _pinned: PinnedRange,
}

impl Pmt {
    /// 逐页设备地址。
    pub(crate) fn addresses(&self) -> &[u64] {
        &self.addresses
    }
}

impl Drop for Pmt {
    fn drop(&mut self) {
        self.bti.backend.unmap(self.bti.stream, &self.addresses);
    }
}

impl KernelObject for Pmt {
    fn object_type(&self) -> ObjectType {
        ObjectType::Pmt
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::INSPECT
    }
}
