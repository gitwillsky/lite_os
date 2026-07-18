use alloc::sync::Arc;

use super::plic_policy::{
    MAX_INTERRUPT_VECTOR, dispatch_claim_batch, enable_word_offset, valid_interrupt_vector,
};
use crate::{
    cpu::{self, CpuSet},
    drivers::{InterruptController, InterruptError, InterruptHandler, InterruptVector},
    fallible_tree::FallibleMap,
};

/// QEMU virt PLIC adapter。hardware context 编码仅存在于 platform backend。
pub(super) struct PlicInterruptController {
    base_addr: usize,
    possible_cpus: CpuSet,
    handlers: FallibleMap<InterruptVector, Arc<dyn InterruptHandler>>,
    affinities: FallibleMap<InterruptVector, CpuSet>,
}

impl PlicInterruptController {
    /// 从 DTB MMIO extent 与已发布 CPU topology 初始化 QEMU virt PLIC adapter。
    ///
    /// # Parameters
    ///
    /// - `base_addr`: PLIC MMIO base address。
    /// - `size`: DTB 声明的 PLIC MMIO extent bytes。
    /// - `possible_cpus`: 已验证且可映射到 supervisor context 的 logical CPUs。
    ///
    /// # Returns
    ///
    /// 初始化 threshold 后的 controller。
    ///
    /// # Errors
    ///
    /// CPU topology 为空、hardware ID 超出 `u32` 或 MMIO extent 无法覆盖标准
    /// priority/enable/context register geometry 时返回 `InterruptError::InvalidVector`；已发布但无法编码
    /// supervisor context 的 topology 违反启动不变量并 fail-stop。
    pub(super) fn new(
        base_addr: usize,
        size: usize,
        possible_cpus: CpuSet,
    ) -> Result<Self, InterruptError> {
        if possible_cpus.is_empty() {
            return Err(InterruptError::InvalidVector);
        }
        let max_hardware_id = possible_cpus
            .iter()
            .map(cpu::hardware_id)
            .map(|id| id.raw())
            .max()
            .ok_or(InterruptError::InvalidVector)?;
        let last_context = Self::supervisor_context(
            u32::try_from(max_hardware_id).map_err(|_| InterruptError::InvalidVector)?,
        );
        let required_context_bytes = 0x200004usize
            .checked_add(last_context as usize * 0x1000)
            .and_then(|offset| offset.checked_add(4));
        let required_priority_bytes = (MAX_INTERRUPT_VECTOR as usize)
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
            possible_cpus,
            handlers: FallibleMap::new(),
            affinities: FallibleMap::new(),
        };
        controller.initialize_hardware();
        Ok(controller)
    }

    fn initialize_hardware(&self) {
        for cpu in self.possible_cpus.iter() {
            self.set_threshold(Self::context_for(cpu), 0);
        }
    }

    #[inline(always)]
    fn context_for(cpu: crate::cpu::CpuId) -> u32 {
        let hardware = cpu::hardware_id(cpu).raw();
        Self::supervisor_context(
            u32::try_from(hardware).expect("PLIC hardware CPU identity exceeds u32"),
        )
    }

    #[inline(always)]
    fn supervisor_context(hardware_cpu: u32) -> u32 {
        hardware_cpu
            .checked_mul(2)
            .and_then(|context| context.checked_add(1))
            .expect("PLIC supervisor context overflow")
    }

    fn priority_offset(&self, vector: u32) -> usize {
        self.base_addr + vector as usize * 4
    }

    fn enable_offset(&self, context: u32) -> usize {
        self.base_addr + 0x2000 + context as usize * 0x80
    }

    fn threshold_offset(&self, context: u32) -> usize {
        self.base_addr + 0x200000 + context as usize * 0x1000
    }

    fn claim_offset(&self, context: u32) -> usize {
        self.base_addr + 0x200004 + context as usize * 0x1000
    }

    fn set_interrupt_priority_raw(&self, vector: u32, priority: u32) {
        let address = self.priority_offset(vector);
        // SAFETY: constructor validates the complete PLIC MMIO extent and caller validates vector.
        unsafe { core::ptr::write_volatile(address as *mut u32, priority) };
    }

    fn enable_for_context(&self, vector: u32, context: u32, enabled: bool) {
        let word_offset =
            enable_word_offset(vector).expect("PLIC vector must be validated before MMIO access");
        let bit = vector % u32::BITS;
        let address = self.enable_offset(context) + word_offset;
        // SAFETY: context comes from discovered CPUs and the validated vector remains within its
        // 0x80-byte enable bitmap instead of crossing into the next context.
        unsafe {
            let current = core::ptr::read_volatile(address as *const u32);
            let replacement = if enabled {
                current | (1 << bit)
            } else {
                current & !(1 << bit)
            };
            core::ptr::write_volatile(address as *mut u32, replacement);
        }
    }

    fn set_threshold(&self, context: u32, threshold: u32) {
        let address = self.threshold_offset(context);
        // SAFETY: context comes from a CPU validated by the constructor.
        unsafe { core::ptr::write_volatile(address as *mut u32, threshold) };
    }

    fn claim(&self, context: u32) -> u32 {
        // SAFETY: current CPU maps to a validated PLIC context; claim is a volatile device read.
        unsafe { core::ptr::read_volatile(self.claim_offset(context) as *const u32) }
    }

    fn complete(&self, context: u32, vector: u32) {
        // SAFETY: current CPU maps to a validated PLIC context; complete is a volatile device write.
        unsafe { core::ptr::write_volatile(self.claim_offset(context) as *mut u32, vector) };
    }
}

impl InterruptController for PlicInterruptController {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError> {
        if !valid_interrupt_vector(vector) {
            return Err(InterruptError::InvalidVector);
        }
        self.handlers
            .try_insert(vector, handler)
            .map_err(|_| InterruptError::NoMemory)?;
        Ok(())
    }

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if !valid_interrupt_vector(vector) {
            return Err(InterruptError::InvalidVector);
        }
        if !self.handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }
        let affinity = self
            .affinities
            .get(&vector)
            .copied()
            .unwrap_or(self.possible_cpus);
        for cpu in self.possible_cpus.iter() {
            self.enable_for_context(vector, Self::context_for(cpu), affinity.contains(cpu));
        }
        Ok(())
    }

    fn set_priority(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        if !valid_interrupt_vector(vector) {
            return Err(InterruptError::InvalidVector);
        }
        self.set_interrupt_priority_raw(vector, 1);
        Ok(())
    }

    fn set_affinity(
        &mut self,
        vector: InterruptVector,
        cpus: CpuSet,
    ) -> Result<(), InterruptError> {
        if !valid_interrupt_vector(vector)
            || cpus.is_empty()
            || cpus.iter().any(|cpu| !self.possible_cpus.contains(cpu))
        {
            return Err(InterruptError::InvalidVector);
        }
        self.affinities
            .try_insert(vector, cpus)
            .map_err(|_| InterruptError::NoMemory)?;
        for cpu in self.possible_cpus.iter() {
            self.enable_for_context(vector, Self::context_for(cpu), cpus.contains(cpu));
        }
        Ok(())
    }

    fn handle_pending_interrupts(&mut self) -> Result<(), InterruptError> {
        let current = cpu::current_id();
        if !self.possible_cpus.contains(current) {
            return Err(InterruptError::InvalidVector);
        }
        let context = Self::context_for(current);
        dispatch_claim_batch(
            || self.claim(context),
            |vector| {
                if !valid_interrupt_vector(vector) {
                    return Err(InterruptError::InvalidVector);
                }
                self.handlers
                    .get(&vector)
                    .cloned()
                    .ok_or(InterruptError::HandlerNotSet)
                    .and_then(|handler| handler.handle_interrupt(vector))
            },
            |vector| self.complete(context, vector),
        )
    }

    fn supports_cpu_affinity(&self) -> bool {
        true
    }
}
