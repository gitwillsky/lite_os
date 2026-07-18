# 内存当前架构

## 当前设计

- frame allocator 唯一拥有物理页容量与 buddy metadata；global allocator 从它临时取得 slab/direct extent，不复制容量 owner。
- `MemorySet` 唯一拥有 page table、program break 与有序 VMA。ELF、stack、anonymous、file、shared/private mapping 使用同一 VMA lifecycle。
- generic memory 只提交 READ/WRITE/EXECUTE/USER/GLOBAL 等语义权限；PTE 编码、canonical address、address-space token 和 local fence 属于 `arch::mmu`。
- user-copy 在 AddressSpace lock 内先完成全范围 fault-in 与权限证明，再复制；不会向 Rust 返回可逃逸的用户 frame reference。
- file mapping range、page-cache resident、private resident、COW 与 futex key 各有单一 owner，OOM 在 publication 前显式返回。
- reclaim 使用有界 cursor 和 fixed batch；页表撤销决定 TLB flush，不能以 frame 最终释放代替 translation invalidation。

## Known limits

- 当前 RISC-V backend 使用 Sv39 且 ASID 为 0；这些不是 generic memory contract。
- 没有 swap，也没有后台 page-cache reclaim/writeback worker。
