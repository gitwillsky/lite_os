pub(super) const GRND_NONBLOCK: usize = 0x1;
pub(super) const GRND_RANDOM: usize = 0x2;
/// Linux 允许在 CRNG 尚未初始化时返回非阻塞字节；LiteOS 的 VirtIO entropy source 在
/// syscall 可达前已经 ready，因此该 bit 与普通请求共享同一已初始化输出路径。缺失该 bit
/// 会让 Rust std `RandomState` 的首选请求错误回退，掩盖 Complete getrandom ABI 偏差。
pub(super) const GRND_INSECURE: usize = 0x4;

/// @description 验证固定 Linux getrandom flags 与互斥组合。
///
/// @param flags userspace 传入的 raw Linux getrandom flags。
/// @return 没有未知 bit 且未组合 `GRND_RANDOM|GRND_INSECURE` 时为 true。
pub(super) const fn getrandom_flags_supported(flags: usize) -> bool {
    let supported = GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE;
    flags & !supported == 0
        && flags & (GRND_RANDOM | GRND_INSECURE) != (GRND_RANDOM | GRND_INSECURE)
}
