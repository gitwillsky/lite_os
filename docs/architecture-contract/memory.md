# 内存契约

## Owner

- frame allocator 独占物理页容量和 buddy metadata；global allocator 独占已借入 extent 内的 slab/direct metadata。
- `MemorySet` 独占 page table、VMA 和 program break；page cache 独占 shared file page、dirty/writeback 与 reclaim state。
- `FilePageRange` 独占 file mapping checked projection；`PrivateResident` 与 `SharedResident` 分别独占对应 residency record。

## Interface

- generic memory 只向 `arch::mmu` 提交语义权限和 frame-owner adapter；PTE bit、address token 与 fence instruction 不得泄漏。
- user-copy 必须先完整证明 range membership、fault 与权限，再复制；不得返回指向 user memory 的 Rust reference。
- 所有 fallible owner storage 必须在 PTE、VMA、cache 或 global registry publication 前 reserve。
- futex key 只能由 AddressSpace identity 或 backing identity + offset 归一化；syscall/task 不得重建 mapping identity。

## Failure and cleanup

- `MAP_FIXED` 等 destructive mutation 必须在所有可前置验证完成后提交；失败不留下部分 unmap、PTE 或 owner publication。
- page-table rollback、TLB invalidation 与 frame release 分开证明；仍被其他 owner 引用的 frame 不得抑制 translation flush。
