use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::fmt;

pub(crate) type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptError {
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

pub(crate) trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
}

pub(crate) trait InterruptController: Send + Sync {
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

pub(crate) struct PlicInterruptController {
    base_addr: usize,
    max_interrupts: u32,
    hart_mask: usize,
    handlers: BTreeMap<InterruptVector, Arc<dyn InterruptHandler>>,
    affinities: BTreeMap<InterruptVector, usize>,
}

impl PlicInterruptController {
    pub(crate) fn new(
        base_addr: usize,
        size: usize,
        max_interrupts: u32,
        hart_mask: usize,
        max_hart_id: usize,
    ) -> Result<Self, InterruptError> {
        if hart_mask == 0
            || max_hart_id >= usize::BITS as usize
            || hart_mask & (1usize << max_hart_id) == 0
            || hart_mask >> max_hart_id != 1
        {
            return Err(InterruptError::InvalidVector);
        }
        let last_context = Self::supervisor_context(max_hart_id as u32);
        let required_context_bytes = 0x200004usize
            .checked_add(last_context as usize * 0x1000)
            .and_then(|offset| offset.checked_add(4));
        let required_priority_bytes = (max_interrupts as usize)
            .checked_add(1)
            .and_then(|count| count.checked_mul(4));
        if base_addr == 0
            || base_addr.checked_add(size).is_none()
            || required_context_bytes.is_none_or(|required| required > size)
            || required_priority_bytes.is_none_or(|required| required > size)
        {
            return Err(InterruptError::InvalidVector);
        }

        let controller = Self {
            base_addr,
            max_interrupts,
            hart_mask,
            handlers: BTreeMap::new(),
            affinities: BTreeMap::new(),
        };

        controller.init_hardware()?;
        Ok(controller)
    }

    fn init_hardware(&self) -> Result<(), InterruptError> {
        let mut harts = self.hart_mask;
        while harts != 0 {
            let hart = harts.trailing_zeros();
            harts &= harts - 1;
            self.set_threshold(Self::supervisor_context(hart), 0);
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

    fn supervisor_context(hart: u32) -> u32 {
        hart * 2 + 1
    }

    fn set_interrupt_priority_raw(&self, vector: u32, priority: u32) {
        if vector == 0 || vector > self.max_interrupts {
            return;
        }

        let addr = self.priority_offset(vector);
        // SAFETY: `new` 已验证 priority 数组位于 DTB PLIC MMIO 区间，vector 也已检查。
        unsafe {
            core::ptr::write_volatile(addr as *mut u32, priority);
        }
    }

    fn enable_interrupt_for_context(&self, vector: u32, context: u32, enable: bool) {
        if vector == 0 || vector > self.max_interrupts {
            return;
        }

        let word = vector / 32;
        let bit = vector % 32;
        let addr = self.enable_offset(context) + (word as usize * 4);

        // SAFETY: context 只由已验证的 hart 生成，vector 在范围内；PLIC MMIO 必须 volatile。
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
        let addr = self.threshold_offset(context);
        // SAFETY: context 只由已验证的 hart 生成，`new` 已检查 context 窗口边界。
        unsafe {
            core::ptr::write_volatile(addr as *mut u32, threshold);
        }
    }

    fn claim_interrupt(&self, context: u32) -> u32 {
        let addr = self.claim_offset(context);
        // SAFETY: context 位于已验证的 PLIC MMIO 区间；claim 是 volatile 设备读取。
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn complete_interrupt(&self, context: u32, vector: u32) {
        let addr = self.claim_offset(context);
        // SAFETY: context 位于已验证的 PLIC MMIO 区间；complete 是 volatile 设备写入。
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

        let affinity = self
            .affinities
            .get(&vector)
            .copied()
            .unwrap_or(self.hart_mask);
        let mut harts = self.hart_mask;
        while harts != 0 {
            let hart = harts.trailing_zeros();
            harts &= harts - 1;
            self.enable_interrupt_for_context(
                vector,
                Self::supervisor_context(hart),
                affinity & (1usize << hart) != 0,
            );
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
        if cpu_mask & !self.hart_mask != 0 {
            return Err(InterruptError::InvalidVector);
        }

        self.affinities.insert(vector, cpu_mask);
        let mut harts = self.hart_mask;
        while harts != 0 {
            let hart = harts.trailing_zeros();
            harts &= harts - 1;
            let enable = (cpu_mask & (1 << hart)) != 0;
            self.enable_interrupt_for_context(vector, Self::supervisor_context(hart), enable);
        }

        Ok(())
    }

    fn handle_pending_interrupts(&mut self) -> Result<(), InterruptError> {
        let hart = crate::arch::hart::hart_id() as u32;
        if self.hart_mask & (1usize << hart) == 0 {
            return Err(InterruptError::InvalidVector);
        }
        let context = Self::supervisor_context(hart);
        let mut first_error = None;
        loop {
            let vector = self.claim_interrupt(context);
            if vector == 0 {
                break;
            }
            // 1. claim 只读取当前 hart 的 S-mode context，不代理其他 hart 消费 IRQ。
            let result = self
                .handlers
                .get(&vector)
                .cloned()
                .ok_or(InterruptError::HandlerNotSet)
                .and_then(|handler| handler.handle_interrupt(vector));
            // 2. 设备 ack 后再 complete，避免 level IRQ 在设备状态未清除时重入。
            self.complete_interrupt(context, vector);
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn supports_cpu_affinity(&self) -> bool {
        true
    }
}
