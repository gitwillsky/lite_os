use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use riscv::register;
use spin::Mutex;

pub type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptError {
    VectorNotFound,
    HandlerNotSet,
    InvalidVector,
    ControllerError,
    ResourceConflict,
    HardwareError,
    TimeoutError,
    InvalidPriority,
    CpuAffinityError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InterruptPriority {
    Critical = 0,
    High = 1,
    Normal = 2,
    Low = 3,
    Idle = 4,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptError::VectorNotFound => write!(f, "Interrupt vector not found"),
            InterruptError::HandlerNotSet => write!(f, "Interrupt handler not set"),
            InterruptError::InvalidVector => write!(f, "Invalid interrupt vector"),
            InterruptError::ControllerError => write!(f, "Interrupt controller error"),
            InterruptError::ResourceConflict => write!(f, "Interrupt resource conflict"),
            InterruptError::HardwareError => write!(f, "Interrupt hardware error"),
            InterruptError::TimeoutError => write!(f, "Interrupt timeout error"),
            InterruptError::InvalidPriority => write!(f, "Invalid interrupt priority"),
            InterruptError::CpuAffinityError => write!(f, "CPU affinity error"),
        }
    }
}

pub trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
    fn can_handle(&self, vector: InterruptVector) -> bool;
    fn priority(&self) -> InterruptPriority {
        InterruptPriority::Normal
    }
    fn is_shared(&self) -> bool {
        false
    }
    fn cpu_affinity(&self) -> Option<usize> {
        None
    }
    fn name(&self) -> &str {
        "unnamed"
    }
    fn statistics(&self) -> InterruptStatistics {
        InterruptStatistics::default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct InterruptStatistics {
    pub total_count: u64,
    pub last_timestamp: u64,
    pub min_latency: u64,
    pub max_latency: u64,
    pub avg_latency: u64,
    pub error_count: u64,
}

pub struct WorkQueue {
    name: alloc::string::String,
    work_items: Mutex<alloc::collections::VecDeque<Box<dyn WorkItem>>>,
    worker_thread: Option<usize>, // Thread ID
    max_items: usize,
}

pub trait WorkItem: Send {
    fn execute(&self) -> Result<(), InterruptError>;
    fn priority(&self) -> InterruptPriority {
        InterruptPriority::Normal
    }
    fn name(&self) -> &str;
}

pub struct BottomHalf {
    id: u32,
    priority: InterruptPriority,
    handler: Box<dyn Fn() -> Result<(), InterruptError> + Send + Sync>,
    statistics: Mutex<InterruptStatistics>,
}

// 软中断类型在 `crate::softirq` 内定义，这里不重复定义

pub struct SoftIrqManager {
    pending: AtomicU32,
    handlers: [Option<Box<dyn Fn() -> Result<(), InterruptError> + Send + Sync>>; 8],
    statistics: [Mutex<InterruptStatistics>; 8],
}

pub trait InterruptController: Send + Sync {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError>;

    fn unregister_handler(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;
    fn disable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn set_priority(
        &mut self,
        vector: InterruptVector,
        priority: InterruptPriority,
    ) -> Result<(), InterruptError>;
    fn set_affinity(
        &mut self,
        vector: InterruptVector,
        cpu_mask: usize,
    ) -> Result<(), InterruptError>;

    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn pending_interrupts(&self) -> Vec<InterruptVector>;
    fn acknowledge_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn get_statistics(&self, vector: InterruptVector) -> Option<InterruptStatistics>;
    fn supports_msi(&self) -> bool {
        false
    }
    fn supports_cpu_affinity(&self) -> bool {
        false
    }
    fn trigger_softirq(&self, _irq: crate::trap::softirq::SoftIrq) {}
}

pub struct SimpleInterruptHandler<F>
where
    F: Fn(InterruptVector) -> Result<(), InterruptError> + Send + Sync,
{
    handler_fn: F,
    vector: InterruptVector,
}

impl<F> SimpleInterruptHandler<F>
where
    F: Fn(InterruptVector) -> Result<(), InterruptError> + Send + Sync,
{
    pub fn new(vector: InterruptVector, handler_fn: F) -> Self {
        Self { handler_fn, vector }
    }
}

impl<F> InterruptHandler for SimpleInterruptHandler<F>
where
    F: Fn(InterruptVector) -> Result<(), InterruptError> + Send + Sync,
{
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        (self.handler_fn)(vector)
    }

    fn can_handle(&self, vector: InterruptVector) -> bool {
        vector == self.vector
    }
}

pub struct BasicInterruptController {
    handlers: Mutex<BTreeMap<InterruptVector, Arc<dyn InterruptHandler>>>,
    enabled_interrupts: Mutex<alloc::collections::BTreeSet<InterruptVector>>,
}

impl BasicInterruptController {
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(BTreeMap::new()),
            enabled_interrupts: Mutex::new(alloc::collections::BTreeSet::new()),
        }
    }
}

impl InterruptController for BasicInterruptController {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError> {
        let mut handlers = self.handlers.lock();
        handlers.insert(vector, handler);
        Ok(())
    }

    fn unregister_handler(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        let mut handlers = self.handlers.lock();
        handlers.remove(&vector);

        let mut enabled = self.enabled_interrupts.lock();
        enabled.remove(&vector);

        Ok(())
    }

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        let handlers = self.handlers.lock();
        if !handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }

        let mut enabled = self.enabled_interrupts.lock();
        enabled.insert(vector);

        Ok(())
    }

    fn disable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        let mut enabled = self.enabled_interrupts.lock();
        enabled.remove(&vector);
        Ok(())
    }

    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        let enabled = self.enabled_interrupts.lock();
        if !enabled.contains(&vector) {
            return Ok(());
        }

        let handlers = self.handlers.lock();
        if let Some(handler) = handlers.get(&vector) {
            handler.handle_interrupt(vector)
        } else {
            Err(InterruptError::HandlerNotSet)
        }
    }

    fn pending_interrupts(&self) -> Vec<InterruptVector> {
        Vec::new()
    }

    fn acknowledge_interrupt(&mut self, _vector: InterruptVector) -> Result<(), InterruptError> {
        Ok(())
    }

    fn set_priority(
        &mut self,
        _vector: InterruptVector,
        _priority: InterruptPriority,
    ) -> Result<(), InterruptError> {
        Err(InterruptError::InvalidPriority)
    }

    fn set_affinity(
        &mut self,
        _vector: InterruptVector,
        _cpu_mask: usize,
    ) -> Result<(), InterruptError> {
        Err(InterruptError::CpuAffinityError)
    }

    fn get_statistics(&self, _vector: InterruptVector) -> Option<InterruptStatistics> {
        None
    }
}

pub struct PlicInterruptController {
    base_addr: usize,
    max_interrupts: u32,
    num_contexts: u32,
    handlers: Mutex<BTreeMap<InterruptVector, Arc<dyn InterruptHandler>>>,
    enabled_interrupts: Mutex<BTreeMap<u32, u32>>, // context -> enabled_mask
    priorities: Mutex<BTreeMap<InterruptVector, InterruptPriority>>,
    statistics: Mutex<BTreeMap<InterruptVector, InterruptStatistics>>,
}

impl PlicInterruptController {
    pub fn new(
        base_addr: usize,
        max_interrupts: u32,
        num_contexts: u32,
    ) -> Result<Self, InterruptError> {
        if base_addr == 0 {
            return Err(InterruptError::InvalidVector);
        }

        let controller = Self {
            base_addr,
            max_interrupts,
            num_contexts,
            handlers: Mutex::new(BTreeMap::new()),
            enabled_interrupts: Mutex::new(BTreeMap::new()),
            priorities: Mutex::new(BTreeMap::new()),
            statistics: Mutex::new(BTreeMap::new()),
        };

        controller.init_hardware()?;
        Ok(controller)
    }

    fn init_hardware(&self) -> Result<(), InterruptError> {
        for vector in 1..=self.max_interrupts {
            self.set_interrupt_priority_raw(vector, 0);
        }

        for context in 0..self.num_contexts {
            self.set_threshold(context, 0);
        }

        Ok(())
    }

    fn priority_offset(&self, vector: u32) -> usize {
        self.base_addr + (vector as usize * 4)
    }

    fn enable_offset(&self, context: u32) -> usize {
        self.base_addr + 0x2000 + (context as usize * 0x80)
    }

    fn threshold_offset(&self, context: u32) -> usize {
        self.base_addr + 0x200000 + (context as usize * 0x1000)
    }

    fn claim_offset(&self, context: u32) -> usize {
        self.base_addr + 0x200004 + (context as usize * 0x1000)
    }

    fn set_interrupt_priority_raw(&self, vector: u32, priority: u32) {
        if vector == 0 || vector > self.max_interrupts {
            return;
        }

        let addr = self.priority_offset(vector);
        unsafe {
            core::ptr::write_volatile(addr as *mut u32, priority);
        }
    }

    fn get_interrupt_priority_raw(&self, vector: u32) -> u32 {
        if vector == 0 || vector > self.max_interrupts {
            return 0;
        }

        let addr = self.priority_offset(vector);
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn enable_interrupt_for_context(&self, vector: u32, context: u32, enable: bool) {
        if vector > self.max_interrupts || context >= self.num_contexts {
            return;
        }

        let word = vector / 32;
        let bit = vector % 32;
        let addr = self.enable_offset(context) + (word as usize * 4);

        unsafe {
            let mut current = core::ptr::read_volatile(addr as *const u32);
            if enable {
                current |= 1 << bit;
            } else {
                current &= !(1 << bit);
            }
            core::ptr::write_volatile(addr as *mut u32, current);
        }
    }

    fn set_threshold(&self, context: u32, threshold: u32) {
        if context >= self.num_contexts {
            return;
        }

        let addr = self.threshold_offset(context);
        unsafe {
            core::ptr::write_volatile(addr as *mut u32, threshold);
        }
    }

    fn claim_interrupt(&self, context: u32) -> u32 {
        if context >= self.num_contexts {
            return 0;
        }

        let addr = self.claim_offset(context);
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn complete_interrupt(&self, context: u32, vector: u32) {
        if context >= self.num_contexts {
            return;
        }

        let addr = self.claim_offset(context);
        unsafe {
            core::ptr::write_volatile(addr as *mut u32, vector);
        }
    }

    pub fn handle_external_interrupt(&self, context: u32) -> Result<(), InterruptError> {
        let vector = self.claim_interrupt(context);
        if vector == 0 {
            return Ok(());
        }

        let start_time = self.get_timestamp();

        let result = {
            let handlers = self.handlers.lock();
            if let Some(handler) = handlers.get(&vector) {
                handler.handle_interrupt(vector)
            } else {
                Err(InterruptError::HandlerNotSet)
            }
        };

        let end_time = self.get_timestamp();
        self.update_statistics(vector, start_time, end_time, result.is_ok());

        self.complete_interrupt(context, vector);

        result
    }

    fn get_timestamp(&self) -> u64 {
        // This would read from a hardware timer in a real implementation
        0
    }

    fn update_statistics(&self, vector: u32, start_time: u64, end_time: u64, success: bool) {
        let mut stats = self.statistics.lock();
        let stat = stats
            .entry(vector)
            .or_insert_with(InterruptStatistics::default);

        stat.total_count += 1;
        stat.last_timestamp = end_time;

        if !success {
            stat.error_count += 1;
        }

        if start_time <= end_time {
            let latency = end_time - start_time;
            if stat.min_latency == 0 || latency < stat.min_latency {
                stat.min_latency = latency;
            }
            if latency > stat.max_latency {
                stat.max_latency = latency;
            }

            stat.avg_latency =
                ((stat.avg_latency * (stat.total_count - 1)) + latency) / stat.total_count;
        }
    }
}

impl InterruptController for PlicInterruptController {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        // 关键：在修改 handlers 表期间禁止本地中断，避免中断上下文中也尝试获取同一把锁导致死锁
        let sie_prev = register::sstatus::read().sie();
        unsafe {
            register::sstatus::clear_sie();
        }
        {
            let mut handlers = self.handlers.lock();
            handlers.insert(vector, handler);
        }
        if sie_prev {
            unsafe {
                register::sstatus::set_sie();
            }
        }
        Ok(())
    }

    fn unregister_handler(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }
        // 同理：在修改 handlers 表期间禁止本地中断
        let sie_prev = register::sstatus::read().sie();
        unsafe {
            register::sstatus::clear_sie();
        }
        {
            let mut handlers = self.handlers.lock();
            handlers.remove(&vector);
        }
        if sie_prev {
            unsafe {
                register::sstatus::set_sie();
            }
        }

        for context in 0..self.num_contexts {
            self.enable_interrupt_for_context(vector, context, false);
        }

        Ok(())
    }

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        let handlers = self.handlers.lock();
        if !handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }

        for context in 0..self.num_contexts {
            self.enable_interrupt_for_context(vector, context, true);
        }

        Ok(())
    }

    fn disable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        for context in 0..self.num_contexts {
            self.enable_interrupt_for_context(vector, context, false);
        }

        Ok(())
    }

    fn set_priority(
        &mut self,
        vector: InterruptVector,
        priority: InterruptPriority,
    ) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        let prio_val = match priority {
            InterruptPriority::Critical => 7,
            InterruptPriority::High => 6,
            InterruptPriority::Normal => 4,
            InterruptPriority::Low => 2,
            InterruptPriority::Idle => 1,
        };

        self.set_interrupt_priority_raw(vector, prio_val);

        let mut priorities = self.priorities.lock();
        priorities.insert(vector, priority);

        Ok(())
    }

    fn set_affinity(
        &mut self,
        vector: InterruptVector,
        cpu_mask: usize,
    ) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        for context in 0..self.num_contexts {
            let enable = (cpu_mask & (1 << context)) != 0;
            self.enable_interrupt_for_context(vector, context, enable);
        }

        Ok(())
    }

    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        let start_time = self.get_timestamp();

        // 关键：在调用实际设备处理函数前释放 handlers 锁，避免与注册/启用中断路径竞争同一把锁导致卡住
        let handler_opt = {
            let handlers = self.handlers.lock();
            handlers.get(&vector).cloned()
        };

        let result = if let Some(handler) = handler_opt {
            handler.handle_interrupt(vector)
        } else {
            Err(InterruptError::HandlerNotSet)
        };

        let end_time = self.get_timestamp();
        self.update_statistics(vector, start_time, end_time, result.is_ok());

        result
    }

    fn pending_interrupts(&self) -> Vec<InterruptVector> {
        let mut pending = Vec::new();

        for context in 0..self.num_contexts {
            let vector = self.claim_interrupt(context);
            if vector != 0 {
                pending.push(vector);
                self.complete_interrupt(context, vector);
            }
        }

        pending
    }

    fn acknowledge_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        for context in 0..self.num_contexts {
            self.complete_interrupt(context, vector);
        }

        Ok(())
    }

    fn get_statistics(&self, vector: InterruptVector) -> Option<InterruptStatistics> {
        let stats = self.statistics.lock();
        stats.get(&vector).cloned()
    }

    fn supports_cpu_affinity(&self) -> bool {
        true
    }
}
