pub(crate) const INIT_PID: usize = 1;
/// Linux futex owner word reserves the top two bits for WAITERS/OWNER_DIED.
pub(super) const PID_MAX: usize = 0x3fff_ffff;

#[derive(Debug)]
pub(super) struct ProcessId(pub(super) usize);

impl ProcessId {
    /// @description 构造当前唯一 init process 的 TGID owner。
    ///
    /// @return 数值为 1 的 ProcessId；新增创建 ABI 前不得增加任意整数构造入口。
    pub(crate) const fn init() -> Self {
        Self(INIT_PID)
    }

    /// @description 由 TaskManager 的单一 PID allocator 构造动态 TGID。
    ///
    /// @param value 大于 INIT_PID 且从未分配过的数值。
    /// @return 新 ProcessId；allocator 违反单调唯一性时由调用方 fail-stop。
    pub(super) const fn allocated(value: usize) -> Self {
        Self(value)
    }
}
