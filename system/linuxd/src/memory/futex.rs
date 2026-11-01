//! futex 等待身份与用户字原子操作。

use core::sync::atomic::{AtomicU32, Ordering};

use super::UserAccessError;

/// 双地址 futex 操作的错误；保留“源地址故障先于目标对齐检查”的 Linux 次序。
#[derive(Debug)]
pub(crate) enum FutexAccessError {
    Unaligned,
    Memory(UserAccessError),
}

impl From<UserAccessError> for FutexAccessError {
    fn from(error: UserAccessError) -> Self {
        Self::Memory(error)
    }
}

/// futex 等待身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FutexKey {
    /// 私有映射：只在同一地址空间内匹配。
    Private {
        address_space: usize,
        address: usize,
    },
    /// 共享映射（匿名共享、文件、设备缓冲）：以后备 VMO 编号与字节偏移跨地址空间匹配。
    Shared { object: u64, offset: u64 },
}

impl FutexKey {
    /// 私有 key 只需地址合法，不读取 VMA（与 Linux `FUTEX_PRIVATE_FLAG` 相同）。
    ///
    /// # Errors
    ///
    /// 未对齐或越过用户地址空间。
    pub(crate) fn private(address: usize, address_space: usize) -> Result<Self, UserAccessError> {
        if address & 3 != 0
            || address
                .checked_add(4)
                .is_none_or(|end| end > super::user_address_end())
        {
            return Err(UserAccessError::Fault);
        }
        Ok(Self::Private {
            address_space,
            address,
        })
    }
}

/// 用户字的标准原子更新；操作数是 syscall 层已完成符号扩展/移位的位模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserAtomicU32Op {
    Set(u32),
    Add(u32),
    Or(u32),
    AndNot(u32),
    Xor(u32),
}

/// 已按 VMA 校验、对齐的用户字；只在持有所属地址空间锁、且当前服务线程运行于该地址空间时存在。
pub(crate) struct UserAtomicU32<'a>(pub(super) &'a AtomicU32);

impl UserAtomicU32<'_> {
    /// 原子比较交换；冲突返回实际旧值。
    pub(crate) fn compare_exchange(&self, current: u32, replacement: u32) -> Result<u32, u32> {
        self.0
            .compare_exchange(current, replacement, Ordering::SeqCst, Ordering::SeqCst)
    }

    /// 原子更新并返回旧值。
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
