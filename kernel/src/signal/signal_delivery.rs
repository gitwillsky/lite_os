use alloc::vec::Vec;
use core::{
    mem::{size_of, align_of},
    ptr::null_mut,
};

use crate::{
    memory::page_table::{translated_ref_mut, translated_byte_buffer},
    task::{TaskControlBlock, current_user_token},
    trap::TrapContext,
};

use super::signal::{Signal, SignalDisposition, SignalFrame, SIG_RETURN_ADDR};

/// 用户栈安全验证器
pub struct UserStackValidator;

impl UserStackValidator {
    /// 栈地址范围常量
    const USER_STACK_MIN: usize = 0x10000;     // 最小用户栈地址
    const USER_STACK_MAX: usize = 0x80000000;  // 最大用户栈地址
    const STACK_RED_ZONE: usize = 4096;        // 栈红区大小（防止栈溢出）
    const MAX_SIGNAL_FRAME_SIZE: usize = 2048; // 最大信号帧大小

    /// 验证用户栈地址是否有效
    pub fn validate_stack_address(sp: usize, required_size: usize) -> Result<(), &'static str> {
        // 检查地址范围
        if sp < Self::USER_STACK_MIN || sp >= Self::USER_STACK_MAX {
            return Err("Stack pointer out of valid range");
        }

        // 检查是否有足够空间（包括红区）
        if sp < Self::USER_STACK_MIN + Self::STACK_RED_ZONE + required_size {
            return Err("Insufficient stack space");
        }

        // 检查信号帧大小是否合理
        if required_size > Self::MAX_SIGNAL_FRAME_SIZE {
            return Err("Signal frame too large");
        }

        // 检查对齐
        if sp % align_of::<usize>() != 0 {
            return Err("Stack pointer not properly aligned");
        }

        Ok(())
    }

    /// 验证用户栈是否可写
    pub fn validate_stack_writable(token: usize, addr: usize, size: usize) -> Result<(), &'static str> {
        // 尝试获取页表转换，验证页面是否存在且可写
        let buffers = translated_byte_buffer(token, addr as *mut u8, size);
        
        if buffers.is_empty() {
            return Err("Stack memory not accessible");
        }

        // 检查是否覆盖了完整的所需空间
        let total_size: usize = buffers.iter().map(|b| b.len()).sum();
        if total_size < size {
            return Err("Stack memory partially inaccessible");
        }

        Ok(())
    }

    /// 计算对齐后的栈帧大小
    pub fn calculate_aligned_frame_size<T>() -> usize {
        let size = size_of::<T>();
        let align = align_of::<T>().max(16); // 至少16字节对齐
        (size + align - 1) & !(align - 1)
    }
}

/// 改进的信号投递引擎
pub struct SafeSignalDelivery;

impl SafeSignalDelivery {
    /// 安全地设置信号处理器
    pub fn setup_signal_handler(
        task: &TaskControlBlock,
        signal: Signal,
        handler_addr: usize,
        handler_info: &SignalDisposition,
        trap_cx: &mut TrapContext,
    ) -> Result<(), &'static str> {
        // 验证处理器地址
        if handler_addr == 0 || handler_addr >= 0x80000000 {
            return Err("Invalid signal handler address");
        }

        // 创建信号帧
        let signal_frame = SignalFrame {
            regs: trap_cx.x,
            pc: trap_cx.sepc,
            status: trap_cx.sstatus.bits(),
            signal: signal as u32,
            return_addr: Self::get_safe_sigreturn_addr(),
        };

        // 获取用户栈指针
        let user_sp = trap_cx.x[2];
        
        // 计算信号帧大小和对齐
        let frame_size = UserStackValidator::calculate_aligned_frame_size::<SignalFrame>();
        let signal_frame_addr = user_sp - frame_size;

        // 验证栈地址和可用性
        UserStackValidator::validate_stack_address(signal_frame_addr, frame_size)?;
        
        let token = task.mm.memory_set.lock().token();
        UserStackValidator::validate_stack_writable(token, signal_frame_addr, frame_size)?;

        // 安全地写入信号帧
        Self::write_signal_frame_safe(token, signal_frame_addr, signal_frame)?;

        // 设置信号掩码（原子操作）
        Self::setup_signal_mask_safe(task, signal, handler_info)?;

        // 修改执行上下文
        trap_cx.sepc = handler_addr;
        trap_cx.x[2] = signal_frame_addr; // 更新栈指针
        trap_cx.x[10] = signal as usize;  // a0: 信号编号
        trap_cx.x[1] = Self::get_safe_sigreturn_addr(); // ra: 返回地址

        debug!("SafeSignalDelivery: Signal {} handler setup complete at {:#x}", 
               signal as u32, handler_addr);

        Ok(())
    }

    /// 安全地写入信号帧
    fn write_signal_frame_safe(
        token: usize,
        addr: usize,
        frame: SignalFrame,
    ) -> Result<(), &'static str> {
        // 使用页表转换安全写入
        let frame_ptr = addr as *mut SignalFrame;
        let frame_ref = translated_ref_mut(token, frame_ptr);
        *frame_ref = frame;

        debug!("SafeSignalDelivery: Signal frame written to {:#x}", addr);
        Ok(())
    }

    /// 安全地设置信号掩码
    fn setup_signal_mask_safe(
        task: &TaskControlBlock,
        signal: Signal,
        handler_info: &SignalDisposition,
    ) -> Result<(), &'static str> {
        // 进入信号处理器前设置掩码
        task.signal_state.lock().enter_signal_handler(handler_info.mask);

        // 如果没有设置 SA_NODEFER，自动阻塞当前信号
        if (handler_info.flags & super::signal::SA_NODEFER) == 0 {
            let mut current_signal_mask = super::signal::SignalSet::new();
            current_signal_mask.add(signal);
            task.signal_state.lock().block_signals(current_signal_mask);
        }

        Ok(())
    }

    /// 获取安全的 sigreturn 地址
    fn get_safe_sigreturn_addr() -> usize {
        // 使用特殊值触发异常，由内核处理
        SIG_RETURN_ADDR
    }

    /// 安全的 sigreturn 实现
    pub fn safe_sigreturn(task: &TaskControlBlock, trap_cx: &mut TrapContext) -> Result<(), &'static str> {
        let user_sp = trap_cx.x[2];
        let signal_frame_addr = user_sp;

        // 验证栈帧地址
        let frame_size = size_of::<SignalFrame>();
        UserStackValidator::validate_stack_address(signal_frame_addr, frame_size)?;

        let token = task.mm.memory_set.lock().token();
        
        // 安全地读取信号帧
        let signal_frame = Self::read_signal_frame_safe(token, signal_frame_addr)?;

        // 验证信号帧完整性
        Self::validate_signal_frame(&signal_frame)?;

        // 恢复寄存器状态
        Self::restore_context_safe(trap_cx, &signal_frame)?;

        // 恢复信号掩码
        task.signal_state.lock().exit_signal_handler();

        debug!("SafeSignalDelivery: Signal {} sigreturn completed", signal_frame.signal);
        
        Ok(())
    }

    /// 安全地读取信号帧
    fn read_signal_frame_safe(token: usize, addr: usize) -> Result<SignalFrame, &'static str> {
        let frame_ptr = addr as *const SignalFrame;
        let frame_ref = translated_ref_mut(token, frame_ptr as *mut SignalFrame);
        Ok(*frame_ref)
    }

    /// 验证信号帧的完整性
    fn validate_signal_frame(frame: &SignalFrame) -> Result<(), &'static str> {
        // 验证信号编号
        if frame.signal == 0 || frame.signal > 31 {
            return Err("Invalid signal number in frame");
        }

        // 验证程序计数器
        if frame.pc == 0 || frame.pc >= 0x80000000 {
            return Err("Invalid program counter in signal frame");
        }

        // 验证栈指针
        if frame.regs[2] < UserStackValidator::USER_STACK_MIN || 
           frame.regs[2] >= UserStackValidator::USER_STACK_MAX {
            return Err("Invalid stack pointer in signal frame");
        }

        // 验证返回地址
        if frame.return_addr != SIG_RETURN_ADDR {
            return Err("Invalid return address in signal frame");
        }

        Ok(())
    }

    /// 安全地恢复执行上下文
    fn restore_context_safe(trap_cx: &mut TrapContext, frame: &SignalFrame) -> Result<(), &'static str> {
        // 恢复通用寄存器
        trap_cx.x = frame.regs;
        trap_cx.sepc = frame.pc;

        // 安全地恢复 sstatus 寄存器
        Self::restore_sstatus_safe(trap_cx, frame.status)?;

        Ok(())
    }

    /// 安全地恢复 sstatus 寄存器
    fn restore_sstatus_safe(trap_cx: &mut TrapContext, saved_status: usize) -> Result<(), &'static str> {
        let mut current_sstatus = riscv::register::sstatus::read();

        // 只恢复安全的状态位
        // SPP (Supervisor Previous Privilege)
        if (saved_status & (1 << 8)) != 0 {
            current_sstatus.set_spp(riscv::register::sstatus::SPP::Supervisor);
        } else {
            current_sstatus.set_spp(riscv::register::sstatus::SPP::User);
        }

        // SPIE (Supervisor Previous Interrupt Enable)
        current_sstatus.set_spie((saved_status & (1 << 5)) != 0);

        // SIE (Supervisor Interrupt Enable) - 谨慎恢复
        current_sstatus.set_sie((saved_status & (1 << 1)) != 0);

        trap_cx.sstatus = current_sstatus;

        Ok(())
    }

    /// 检查信号处理器地址的有效性
    pub fn validate_handler_address(addr: usize) -> Result<(), &'static str> {
        if addr == 0 {
            return Err("NULL signal handler address");
        }

        if addr == 1 {
            return Ok(()); // SIG_IGN
        }

        if addr >= 0x80000000 {
            return Err("Signal handler in kernel space");
        }

        // 检查地址对齐
        if addr % 4 != 0 {
            return Err("Signal handler address not aligned");
        }

        Ok(())
    }

    /// 计算信号处理所需的栈空间
    pub fn calculate_required_stack_space() -> usize {
        UserStackValidator::calculate_aligned_frame_size::<SignalFrame>() + 
        UserStackValidator::STACK_RED_ZONE
    }
}

/// 信号投递错误类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalDeliveryError {
    InvalidStackAddress,
    InsufficientStackSpace,
    InvalidHandlerAddress,
    MemoryAccessError,
    InvalidSignalFrame,
    PermissionDenied,
}

impl core::fmt::Display for SignalDeliveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidStackAddress => write!(f, "Invalid stack address"),
            Self::InsufficientStackSpace => write!(f, "Insufficient stack space"),
            Self::InvalidHandlerAddress => write!(f, "Invalid handler address"),
            Self::MemoryAccessError => write!(f, "Memory access error"),
            Self::InvalidSignalFrame => write!(f, "Invalid signal frame"),
            Self::PermissionDenied => write!(f, "Permission denied"),
        }
    }
}