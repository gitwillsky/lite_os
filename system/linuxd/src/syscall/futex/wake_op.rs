//! Linux FUTEX_OP 位域 codec；原子 word 与实际队列仍由 memory/task owner 消费。
use crate::{execution::task::FutexWaitError, memory::UserAtomicU32Op};

pub(super) struct WakeOperation {
    pub(super) update: Option<UserAtomicU32Op>,
    comparison: u8,
    argument: i32,
}
impl WakeOperation {
    /// 固定 12-bit signed 操作数；SHIFT 按 native legacy 语义把位数投影到 0..31。
    /// 不提前拒绝 comparison：Linux 在 actual atomic update 后才返回 unsupported comparison。
    pub(super) fn decode(encoded: u32) -> Self {
        let signed_operand = ((encoded << 8) as i32) >> 20;
        let operand = if encoded & 0x8000_0000 != 0 {
            1u32 << (signed_operand as u32 & 31)
        } else {
            signed_operand as u32
        };
        let update = match (encoded >> 28) & 7 {
            0 => Some(UserAtomicU32Op::Set(operand)),
            1 => Some(UserAtomicU32Op::Add(operand)),
            2 => Some(UserAtomicU32Op::Or(operand)),
            3 => Some(UserAtomicU32Op::AndNot(operand)),
            4 => Some(UserAtomicU32Op::Xor(operand)),
            _ => None,
        };
        Self {
            update,
            comparison: ((encoded >> 24) & 15) as u8,
            argument: ((encoded << 20) as i32) >> 20,
        }
    }
    /// 比较 actual RMW 返回的旧 signed word；不比较更新后的值。
    pub(super) fn compare(&self, old: u32) -> Result<bool, FutexWaitError> {
        let old = old as i32;
        Ok(match self.comparison {
            0 => old == self.argument,
            1 => old != self.argument,
            2 => old < self.argument,
            3 => old >= self.argument,
            4 => old <= self.argument,
            5 => old > self.argument,
            _ => return Err(FutexWaitError::Unsupported),
        })
    }
}
