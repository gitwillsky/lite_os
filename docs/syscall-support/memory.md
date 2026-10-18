# Virtual memory syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 214 | `brk` | Complete | program break 与 anonymous VMA 统一 owner |
| 215 | `munmap` | Complete | VMA split、shared writeback 与 TLB cleanup |
| 216 | `mremap` | Partial | 私有/共享匿名、私有/共享文件与 device 映射的缩小、增长、`MAYMOVE`/`FIXED`/`DONTUNMAP` 搬迁和共享映射别名（`old_size == 0`）；stack 与 loader System VMA 返回 EFAULT |
| 222 | `mmap` | Partial | anonymous/regular-file/memfd/ALSA PCM、private/shared、fixed，以及 noreserve/stack advisory；ALSA 只接受 writable `MAP_SHARED` playback buffer |
| 224 | `swapon` | Partial | SWAPSPACE2 v1 block/file swap、priority flags、统一 private/sharedAnon/tmpfs/COW 页 owner；共享页双架构1/2CPU阶段验证通过，完整产品门禁待执行 |
| 225 | `swapoff` | Partial | 预备 waiter、禁止新槽、恢复物理 backing 与已登记 mm 的逻辑 epoch，失败保留 lease；page/MM work unit 之间及最终槽退休等待支持 EINTR 重试 |
| 226 | `mprotect` | Complete | Linux protection combinations 与 VMA split |
| 227 | `msync` | Partial | shared regular-file mapping 的同步范围 |
| 233 | `madvise` | Partial | 已声明 advice、discard/reclaim 与 residency 语义 |

## 已知缺口

`mremap` 搬迁转移 VMA 的全部 owner（resident 页、共享 backing、文件范围）并整体换到新地址，不复制页；旧地址的翻译由同步 fence 封闭，新地址在首次访问时按已有 owner 重新安装。
共享匿名与共享文件映射增长出的页没有 backing，访问得到 SIGBUS，与 Linux 一致；device 映射不可增长（EFAULT）。
源范围必须被单个 VMA 完整覆盖，否则 EFAULT；stack 的大小由 stack owner 管理，不可 remap。

swapon(224)/swapoff(225) 已接入标准 Linux v1 block/file swap，用户 private、共享 COW、
共享匿名与 tmpfs/memfd 统一迁移；共享页路径已通过双架构各1/2CPU TCG阶段验证；完整产品门禁待全部范围完成后执行。没有 commit accounting、huge page、NUMA 或 `userfaultfd`。通用后台压力回收已实现，
采用有界采样与低/高水位，不声明完整 Linux kswapd/zone policy。
ext4 已有定期 journal/page-cache 写回线程；它不提供通用脏页压力控制。
当前 backend 的 Sv39/ASID 细节不属于本 ABI contract。

- `swapon`/`swapoff`：要求 calling Thread 的 CAP_SYS_ADMIN；当前 initial user namespace，尚未实现跨 user namespace 的 scoping。
  `swapon` 验证 SWAPSPACE2 v1 header、实际容量、坏页及标准 priority flags；discard 是可选
  advisory，未伪造块设备 discard 支持。最多32个 active area，同 priority 轮转分配。
  mount/raw writer/swap 共用物理 extent exclusion。`swapoff` 先禁止新槽，再恢复全部已登记
  canonical 物理 backing 和 mm 的对应 logical epoch；ENOMEM/EIO 保留剩余槽和设备占用，
  成功返回前同步归还 claim。`/proc/swaps`、meminfo 与 sysinfo 从同一 area owner 投影。
  page-in 缓存一份 Resident 与未恢复 alias 所需的唯一 retained slot，全部 alias 恢复后释放；
  `/proc/meminfo` 的 SwapCached 按唯一 Resident+retained slot capability 计数，提供近似
  vmstat 快照；`/proc/PID/status` 的 VmSwap 只计 private anonymous，排除 shared anonymous/tmpfs。

- ext4 swapfile 使用4KiB固定profile、非稀疏 allocated extents（含 unwritten），先撤共享 writer、
  排空 cache/journal，再直接 extent I/O。O_RDWR 来源阻 readonly/remount,ro，opened pin umount；
  write/truncate/fallocate、新 shared W mmap 返回 ETXTBSY，unlink/rename source/target EPERM。
  mprotect 保留原 fd VM_MAYWRITE；冻结后的旧 shared write SIGBUS，private/read-only 仍可映射。

swapoff 的可交付信号在 physical page/MM work unit 之间检查，最后槽退休使用可中断
TaskEvent；空 area 或最后槽已经释放优先成功。EINTR 恢复 Active、保留已恢复内容与
剩余槽/storage lease；PageOp/MM lock 与单次同步 I/O 内部等待不会提前中断。
