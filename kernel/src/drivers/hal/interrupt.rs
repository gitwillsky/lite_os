use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::fmt;

pub type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptError {
    HandlerNotSet,
    InvalidVector,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptError::HandlerNotSet => write!(f, "Interrupt handler not set"),
            InterruptError::InvalidVector => write!(f, "Invalid interrupt vector"),
        }
    }
}

pub trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
}

pub trait InterruptController: Send + Sync {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError>;

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn set_priority(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;
    fn set_affinity(
        &mut self,
        vector: InterruptVector,
        cpu_mask: usize,
    ) -> Result<(), InterruptError>;

    fn handle_pending_interrupts(&mut self) -> Result<(), InterruptError>;
    fn supports_cpu_affinity(&self) -> bool {
        false
    }
}

pub struct PlicInterruptController {
    base_addr: usize,
    max_interrupts: u32,
    num_contexts: u32,
    handlers: BTreeMap<InterruptVector, Arc<dyn InterruptHandler>>,
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
            handlers: BTreeMap::new(),
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

        self.handlers.insert(vector, handler);
        Ok(())
    }

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        if !self.handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }

        for context in 0..self.num_contexts {
            self.enable_interrupt_for_context(vector, context, true);
        }

        Ok(())
    }

    fn set_priority(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if vector == 0 || vector > self.max_interrupts {
            return Err(InterruptError::InvalidVector);
        }

        // LiteOS 当前只有启动所需 block IRQ，使用最低非零 priority 即可。
        self.set_interrupt_priority_raw(vector, 1);

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

    fn handle_pending_interrupts(&mut self) -> Result<(), InterruptError> {
        let mut first_error = None;
        for context in 0..self.num_contexts {
            loop {
                let vector = self.claim_interrupt(context);
                if vector == 0 {
                    break;
                }
                // 1. 只在查表期间持 handlers 锁；设备 handler 不得反向依赖该表。
                let handler = self.handlers.get(&vector).cloned();
                // 2. handler 在无 PLIC 内部锁时执行，当前实现只做设备 MMIO ack。
                let result = handler
                    .ok_or(InterruptError::HandlerNotSet)
                    .and_then(|handler| handler.handle_interrupt(vector));
                // 3. handler 完成后再通知 PLIC；提前 complete 会允许同一 IRQ 在状态未清除时重入。
                self.complete_interrupt(context, vector);
                if first_error.is_none() {
                    first_error = result.err();
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn supports_cpu_affinity(&self) -> bool {
        true
    }
}
