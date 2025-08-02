//! 信号投递机制

use super::core::{Signal, SignalError};
use crate::{
    memory::page_table::{translated_byte_buffer, translated_ref_mut},
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

    // 修改执行上下文，跳转到信号处理器
    trap_cx.sepc = handler_addr; // PC指向处理器
    trap_cx.x[2] = signal_frame_addr; // 更新栈指针
    trap_cx.x[10] = signal as usize; // a0: 信号编号
    trap_cx.x[1] = 0; // ra: 特殊返回地址

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
    let user_sp = trap_cx.x[2];

    // 读取信号帧
    let signal_frame = read_signal_frame(task, user_sp)?;

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

    let frame_ptr = translated_ref_mut::<SignalFrame>(token, addr as *mut SignalFrame);
    Ok(*frame_ptr)
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
