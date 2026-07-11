//! 信号投递机制

use super::core::{Signal, SignalError};
use crate::{task::TaskControlBlock, trap::TrapContext};
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

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<usize>() == 8);
    assert!(offset_of!(SignalFrame, pc) == 256);
    assert!(offset_of!(SignalFrame, status) == 264);
    assert!(offset_of!(SignalFrame, signal) == 272);
    assert!(offset_of!(SignalFrame, return_addr) == 280);
    assert!(size_of::<SignalFrame>() == 288);
};

/// 设置信号处理器执行环境
pub fn setup_signal_handler(
    task: &TaskControlBlock,
    signal: Signal,
    handler_addr: usize,
    trap_cx: &mut TrapContext,
) -> Result<(), SignalError> {
    // 验证处理器地址
    if !task.is_user_executable(handler_addr) {
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
    let signal_frame_addr = user_sp
        .checked_sub(frame_size)
        .ok_or(SignalError::InvalidAddress)?;

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
    trap_cx.x[2] = signal_frame_addr; // frame 大小为 16-byte 倍数，保持 psABI 栈对齐
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
        task.tgid()
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
    if !task.is_user_executable(signal_frame.pc) {
        return Err(SignalError::InvalidAddress);
    }

    // 恢复寄存器状态
    trap_cx.x = signal_frame.regs;
    trap_cx.sepc = signal_frame.pc;
    let mut restored_status = riscv::register::sstatus::Sstatus::from_bits(signal_frame.status);
    restored_status.set_spp(riscv::register::sstatus::SPP::User);
    restored_status.set_sie(false);
    restored_status.set_spie(true);
    restored_status.set_sum(false);
    restored_status.set_mxr(false);
    trap_cx.sstatus = restored_status;

    debug!(
        "Signal {} sigreturn completed for PID {}",
        signal_frame.signal,
        task.tgid()
    );

    Ok(())
}

/// 写入信号帧到用户内存
fn write_signal_frame(task: &TaskControlBlock, addr: usize, frame: &SignalFrame) -> Result<(), ()> {
    let bytes = encode_signal_frame(frame);
    task.copy_to_user(addr, &bytes).map_err(|_| ())
}

/// 从用户内存读取信号帧
fn read_signal_frame(task: &TaskControlBlock, addr: usize) -> Result<SignalFrame, SignalError> {
    let mut bytes = [0u8; size_of::<SignalFrame>()];
    task.copy_from_user(addr, &mut bytes)
        .map_err(|_| SignalError::InvalidAddress)?;
    Ok(decode_signal_frame(&bytes))
}

fn encode_signal_frame(frame: &SignalFrame) -> [u8; size_of::<SignalFrame>()] {
    let mut bytes = [0u8; size_of::<SignalFrame>()];
    for (index, register) in frame.regs.iter().enumerate() {
        let start = index * size_of::<usize>();
        bytes[start..start + size_of::<usize>()].copy_from_slice(&register.to_ne_bytes());
    }
    bytes[256..264].copy_from_slice(&frame.pc.to_ne_bytes());
    bytes[264..272].copy_from_slice(&frame.status.to_ne_bytes());
    bytes[272..276].copy_from_slice(&frame.signal.to_ne_bytes());
    bytes[280..288].copy_from_slice(&frame.return_addr.to_ne_bytes());
    bytes
}

fn decode_signal_frame(bytes: &[u8; size_of::<SignalFrame>()]) -> SignalFrame {
    let mut regs = [0usize; 32];
    for (index, register) in regs.iter_mut().enumerate() {
        let start = index * size_of::<usize>();
        *register =
            usize::from_ne_bytes(bytes[start..start + size_of::<usize>()].try_into().unwrap());
    }
    SignalFrame {
        regs,
        pc: usize::from_ne_bytes(bytes[256..264].try_into().unwrap()),
        status: usize::from_ne_bytes(bytes[264..272].try_into().unwrap()),
        signal: u32::from_ne_bytes(bytes[272..276].try_into().unwrap()),
        return_addr: usize::from_ne_bytes(bytes[280..288].try_into().unwrap()),
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
    let user_end = 1usize << (crate::memory::VIRTUAL_ADDRESS_WIDTH - 1);
    if frame.pc == 0 || frame.pc >= user_end {
        return Err(SignalError::InvalidAddress);
    }

    Ok(())
}
