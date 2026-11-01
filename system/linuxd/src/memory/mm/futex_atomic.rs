//! 已验证用户 u32 的实际原子 owner；raw FUTEX_OP codec 不进入 memory domain。
use core::sync::atomic::{AtomicU32, Ordering};

/// 用户 word 的标准原子更新；操作数是 syscall 已完成 sign-extension/shift 的位模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserAtomicU32Op {
    Set(u32),
    Add(u32),
    Or(u32),
    AndNot(u32),
    Xor(u32),
}

/// 借用已 fault-in、可写且页内对齐的 word；不能活过实际 MemorySet guard。
pub(crate) struct UserAtomicU32<'a>(pub(super) &'a AtomicU32);
impl UserAtomicU32<'_> {
    /// 原生 futex/robust 的原子 owner transfer；冲突返回真实旧值供事务重试。
    pub(crate) fn compare_exchange(&self, current: u32, replacement: u32) -> Result<u32, u32> {
        self.0
            .compare_exchange(current, replacement, Ordering::SeqCst, Ordering::SeqCst)
    }

    /// 原子更新实际共享物理 word 并返回旧值；不是 copyin/计算/copyout 的非原子循环。
    pub(crate) fn update(&self, operation: UserAtomicU32Op) -> u32 {
        match operation {
            UserAtomicU32Op::Set(value) => self.0.swap(value, Ordering::SeqCst),
            UserAtomicU32Op::Add(value) => self.0.fetch_add(value, Ordering::SeqCst),
            UserAtomicU32Op::Or(value) => self.0.fetch_or(value, Ordering::SeqCst),
            UserAtomicU32Op::AndNot(value) => self.0.fetch_and(!value, Ordering::SeqCst),
            UserAtomicU32Op::Xor(value) => self.0.fetch_xor(value, Ordering::SeqCst),
        }
    }
}
