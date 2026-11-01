//! 逻辑 CPU 标识。
//!
//! linuxd 不拥有 CPU：逻辑 CPU 编号与内核一致（`0..count`），当前 CPU 由 `CPU_CURRENT` 查询，
//! 只作提示（返回时线程可能已迁移），与 Linux `getcpu` 的语义相同。

use crate::sync::Once;

/// 逻辑 CPU 编号。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CpuId(usize);

impl CpuId {
    /// 数组下标与 Linux CPU number。
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

/// 逻辑 CPU 集合（bit N = CPU N）。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuSet(usize);

impl CpuSet {
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = CpuId> {
        let mut bits = self.0;
        core::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let index = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(CpuId(index))
        })
    }

    /// 机器字位图（Linux CPU mask 的 native word）。
    pub(crate) fn native_word(self) -> usize {
        self.0
    }

    /// 由机器字位图构造。
    pub(crate) fn from_native_word(word: usize) -> Self {
        Self(word)
    }
}

impl core::ops::BitAnd for CpuSet {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// 逻辑 CPU 数量。
pub(crate) fn count() -> usize {
    // OWNER: 内核 CPU 拓扑在启动后不变。
    static COUNT: Once<usize> = Once::new();
    *COUNT.call_once(|| crate::system_info().cpu_count as usize)
}

/// 第 `index` 个逻辑 CPU。
pub(crate) fn id_at(index: usize) -> Option<CpuId> {
    (index < count()).then_some(CpuId(index))
}

/// 全部逻辑 CPU。
pub(crate) fn possible() -> CpuSet {
    CpuSet(if count() >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << count()) - 1
    })
}

/// 在线 CPU（内核启动后全部在线）。
pub(crate) fn online() -> CpuSet {
    possible()
}

/// 可调度 CPU。
pub(crate) fn active() -> CpuSet {
    possible()
}

/// 调用线程当前所在的 CPU（提示）。
pub(crate) fn current_id() -> CpuId {
    CpuId(native_runtime::system::current_cpu() as usize)
}
