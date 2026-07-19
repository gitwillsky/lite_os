pub(super) const MAP_SHARED: usize = 0x01;
pub(super) const MAP_PRIVATE: usize = 0x02;
pub(super) const MAP_FIXED: usize = 0x10;
pub(super) const MAP_ANONYMOUS: usize = 0x20;
/// Linux advisory：不预留 swap/commit；LiteOS 没有 reservation accounting owner。
pub(super) const MAP_NORESERVE: usize = 0x4000;
/// Linux advisory：mapping 预期作为 Thread stack；LiteOS 不需要保存该 VMA hint。
/// 缺失时 Rust std 在进入 `main` 前建立 stack-overflow guard 会收到 `EINVAL`。
pub(super) const MAP_STACK: usize = 0x2_0000;
pub(super) const MAP_FIXED_NOREPLACE: usize = 0x10_0000;

const SUPPORTED_FLAGS: usize = MAP_PRIVATE
    | MAP_SHARED
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_NORESERVE
    | MAP_STACK
    | MAP_FIXED_NOREPLACE;

/// @description 验证 Linux mmap sharing 与当前开放的 semantic/advisory flags。
///
/// @param flags userspace 传入的 raw Linux mmap flags。
/// @return 恰有一个 sharing mode、没有未知 bit 且 fixed variants 不冲突时为 true。
pub(super) const fn mmap_flags_supported(flags: usize) -> bool {
    let sharing = flags & (MAP_PRIVATE | MAP_SHARED);
    matches!(sharing, MAP_PRIVATE | MAP_SHARED)
        && flags & !SUPPORTED_FLAGS == 0
        && !(flags & MAP_FIXED != 0 && flags & MAP_FIXED_NOREPLACE != 0)
}
