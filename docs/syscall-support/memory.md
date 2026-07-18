# Virtual memory syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 214 | `brk` | Complete | program break 与 anonymous VMA 统一 owner |
| 215 | `munmap` | Complete | VMA split、shared writeback 与 TLB cleanup |
| 222 | `mmap` | Partial | anonymous/file、private/shared、fixed 与 noreserve advisory |
| 226 | `mprotect` | Complete | Linux protection combinations 与 VMA split |
| 227 | `msync` | Partial | shared regular-file mapping 的同步范围 |
| 233 | `madvise` | Partial | 已声明 advice、discard/reclaim 与 residency 语义 |

## 已知缺口

没有 swap、commit accounting、huge page、NUMA、`userfaultfd` 或后台 reclaim/writeback。当前 backend 的 Sv39/ASID 细节不属于本 ABI contract。
