pub(crate) const INIT_PID: usize = 1;

#[derive(Debug)]
pub(super) struct ProcessId(pub(super) usize);

impl ProcessId {
    /// @description 构造当前唯一 init process 的 TGID owner。
    ///
    /// @return 数值为 1 的 ProcessId；新增创建 ABI 前不得增加任意整数构造入口。
    pub(crate) const fn init() -> Self {
        Self(INIT_PID)
    }
}
