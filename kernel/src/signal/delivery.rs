//! 信号投递机制

use super::core::{Signal, SignalError};
use crate::{
    memory::{
        address::VirtualAddress,
        page_table::{PageTable, translated_byte_buffer},
    },
    task::TaskControlBlock,
    trap::TrapContext,
};
use core::mem::size_of;

/// 保存在用户栈上的信号帧
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SignalFrame {
    /// 保存的寄存器状态
    pub regs: [usize; 32],
    /// 保存的程序计数器
    pub pc: usize,
    /// 保存的状态寄存器
    pub status: usize,
    /// 信号编号
    pub signal: u32,
    /// 返回地址（用于sigreturn）
    pub return_addr: usize,
}

/// 设置信号处理器执行环境
pub fn setup_signal_handler(
    task: &TaskControlBlock,
    signal: Signal,
    handler_addr: usize,
    trap_cx: &mut TrapContext,
) -> Result<(), SignalError> {
    // 验证处理器地址
    if handler_addr == 0 || handler_addr >= 0x80000000 {
        return Err(SignalError::InvalidAddress);
    }

    // 创建信号帧
    let signal_frame = SignalFrame {
        regs: trap_cx.x,
        pc: trap_cx.sepc,
        status: trap_cx.sstatus.bits(),
        signal: signal as u32,
        return_addr: 0, // 特殊值，用于触发sigreturn
    };

    // 获取用户栈指针并预留空间
    let user_sp = trap_cx.x[2];
    let frame_size = size_of::<SignalFrame>();
    let signal_frame_addr = user_sp - frame_size;

    // 检查栈地址合法性
    if signal_frame_addr == 0 {
        return Err(SignalError::InvalidAddress);
    }

    // 写入信号帧到用户栈
    if let Err(_) = write_signal_frame(task, signal_frame_addr, &signal_frame) {
        return Err(SignalError::InternalError);
    }

    // 为信号处理函数预留更多栈空间
    let handler_sp = signal_frame_addr - 1024; // 预留1KB栈空间给信号处理函数

    // 修改执行上下文，跳转到信号处理器
    trap_cx.sepc = handler_addr; // PC指向处理器
    trap_cx.x[2] = handler_sp; // 更新栈指针，为信号处理函数预留栈空间
    trap_cx.x[10] = signal as usize; // a0: 信号编号
    // 将信号帧地址通过 a1 传递给用户态处理器，便于用户代码需要时获取
    trap_cx.x[11] = signal_frame_addr; // a1: 指向 SignalFrame 的用户地址（非可靠用于返回）
    // 同时把地址写入一个“被调用者保存”的寄存器（如 s11/x27），
    // 以保证在函数返回时仍能恢复到原值，便于内核在 sigreturn 时可靠取回
    trap_cx.x[27] = signal_frame_addr; // s11: callee-saved，作为 sigreturn 的权威来源
    trap_cx.x[1] = 0; // ra: 特殊返回地址，触发sigreturn

    debug!(
        "Signal {} handler setup at {:#x} for PID {}",
        signal as u32,
        handler_addr,
        task.pid()
    );

    Ok(())
}

/// 从信号处理函数返回
pub fn sig_return(task: &TaskControlBlock, trap_cx: &mut TrapContext) -> Result<(), SignalError> {
    // 优先从 callee-saved 的 s11(x27) 读取信号帧地址，最可靠
    let mut signal_frame_addr = trap_cx.x[27];
    if signal_frame_addr == 0 {
        // 退而求其次：尝试 a1（可能被用户代码改写，非可靠）
        signal_frame_addr = trap_cx.x[11];
    }

    // 读取信号帧
    let signal_frame = read_signal_frame(task, signal_frame_addr)?;

    // 验证信号帧完整性
    validate_signal_frame(&signal_frame)?;

    // 恢复寄存器状态
    trap_cx.x = signal_frame.regs;
    trap_cx.sepc = signal_frame.pc;
    trap_cx.sstatus = riscv::register::sstatus::Sstatus::from_bits(signal_frame.status);

    debug!(
        "Signal {} sigreturn completed for PID {}",
        signal_frame.signal,
        task.pid()
    );

    Ok(())
}

/// 写入信号帧到用户内存
fn write_signal_frame(task: &TaskControlBlock, addr: usize, frame: &SignalFrame) -> Result<(), ()> {
    let token = task.mm.memory_set.lock().token();
    let frame_bytes = unsafe {
        core::slice::from_raw_parts(
            frame as *const SignalFrame as *const u8,
            size_of::<SignalFrame>(),
        )
    };

    let buffers = translated_byte_buffer(token, addr as *const u8, frame_bytes.len());
    let mut offset = 0;
    for buffer in buffers {
        let len = buffer.len().min(frame_bytes.len() - offset);
        buffer[..len].copy_from_slice(&frame_bytes[offset..offset + len]);
        offset += len;
    }
    Ok(())
}

/// 从用户内存读取信号帧
fn read_signal_frame(task: &TaskControlBlock, addr: usize) -> Result<SignalFrame, SignalError> {
    let token = task.mm.memory_set.lock().token();
    let page_table = PageTable::from_token(token);
    let va = VirtualAddress::from(addr);
    if let Some(pa) = page_table.translate_va(va) {
        let frame_ptr: &mut SignalFrame = pa.get_mut();
        Ok(*frame_ptr)
    } else {
        Err(SignalError::InvalidAddress)
    }
}

/// 验证信号帧的完整性
fn validate_signal_frame(frame: &SignalFrame) -> Result<(), SignalError> {
    // 检查信号编号是否有效
    if frame.signal == 0 || frame.signal > 31 {
        return Err(SignalError::InvalidSignal);
    }

    // 检查返回地址是否为特殊值
    if frame.return_addr != 0 {
        return Err(SignalError::InternalError);
    }

    // 检查PC地址是否合理（用户空间）
    if frame.pc >= 0x80000000 {
        return Err(SignalError::InvalidAddress);
    }

    Ok(())
}
