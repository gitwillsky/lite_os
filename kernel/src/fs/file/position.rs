use spin::Mutex;

/// @description Linux open file description 的共享 file position owner。
///
/// 所有依赖并推进同一 position 的操作都必须通过 `with` 保持一个临界区；只做
/// snapshot 再另行写回会让 fork/dup 后的并发 I/O 丢失进度。双 OFD 操作使用
/// `with_pair` 的地址全序，缺失该顺序会让反向 sendfile 形成 ABBA。
pub(super) struct FilePosition(Mutex<u64>);

impl FilePosition {
    /// @description 创建从零开始的 OFD position。
    /// @return 不分配、初值为零的 position owner。
    pub(super) const fn new() -> Self {
        Self(Mutex::new(0))
    }

    /// @description 在该 OFD position 的唯一临界区内执行一次完整操作。
    ///
    /// `operation` 可读取并推进 position；返回时所有变化一次性对其他共享该 OFD
    /// 的 descriptor 可见。
    /// @param operation 依赖并可推进当前 position 的完整 operation。
    /// @return operation 的原始返回值。
    pub(super) fn with<R>(&self, operation: impl FnOnce(&mut u64) -> R) -> R {
        operation(&mut self.0.lock())
    }

    /// @description 返回不推进 position 的瞬时快照。
    /// @return 当前共享 position。
    pub(super) fn snapshot(&self) -> u64 {
        *self.0.lock()
    }

    /// @description 原子计算并发布一个 Linux signed file position。
    ///
    /// `base` 在 position 临界区内收到当前值，可选择 SEEK_SET/CUR/END 的领域基准。
    /// 负结果或超出 signed `loff_t` 的结果被拒绝，失败时原 position 保持不变。
    /// @param offset signed byte delta。
    /// @param base 把当前 position 投影为本次 seek 基准的 closure。
    /// @return 成功发布的新 position。
    /// @errors 结果为负或超出 `i64::MAX` 时返回错误。
    pub(super) fn seek(&self, offset: i64, base: impl FnOnce(u64) -> u64) -> Result<u64, ()> {
        self.with(|position| {
            let value = i128::from(base(*position)) + i128::from(offset);
            if !(0..=i128::from(i64::MAX)).contains(&value) {
                return Err(());
            }
            *position = value as u64;
            Ok(*position)
        })
    }

    /// @description 按稳定地址全序锁定两个不同的 OFD positions。
    ///
    /// closure 参数顺序始终与 `first`、`second` 一致，而非实际加锁顺序。两个参数
    /// 指向同一 owner 时返回 `None`，避免递归获取同一 non-reentrant mutex。
    /// @param first caller 语义中的第一个 position。
    /// @param second caller 语义中的第二个 position。
    /// @param operation 同时依赖并可推进两个 positions 的完整 operation。
    /// @return 两个 owner 不同时返回 operation 结果；相同时返回 `None`。
    pub(super) fn with_pair<R>(
        first: &Self,
        second: &Self,
        operation: impl FnOnce(&mut u64, &mut u64) -> R,
    ) -> Option<R> {
        if core::ptr::eq(first, second) {
            return None;
        }
        if (first as *const Self as usize) < (second as *const Self as usize) {
            let mut first = first.0.lock();
            let mut second = second.0.lock();
            Some(operation(&mut first, &mut second))
        } else {
            let mut second = second.0.lock();
            let mut first = first.0.lock();
            Some(operation(&mut first, &mut second))
        }
    }
}
