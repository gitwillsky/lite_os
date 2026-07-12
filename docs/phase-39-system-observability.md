# Phase 39：系统可观测性竖切

## 目标

以一个中立系统快照同时支撑 Linux `sysinfo(179)` 与现有 procfs，并让固定动态 BusyBox 的 `ps/free/uptime` 成为真实 consumer。禁止新增统计 owner、解析 `/proc` 回填 syscall，或保留另一套兼容入口。

## 固定规范

- Linux `v7.1` commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`：`include/uapi/linux/sysinfo.h`、`kernel/sys.c::do_sysinfo` 与 asm-generic syscall table。
- musl `v1.2.6` commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`：`include/sys/sysinfo.h` 与 `src/linux/sysinfo.c`。
- BusyBox `1.37.0`：`free`、`uptime` 直接消费 `sysinfo`；基础 `ps` 消费 `/proc` process snapshot。

RV64 `struct sysinfo` 的 kernel copyout 长度为 112 bytes。字段 offset 固定为 uptime 0、loads 8、RAM/swap 32..72、procs 80、highmem 88/96、mem_unit 104；padding 必须为零。musl 尾部 reserved 数组不改变 kernel UAPI copyout 长度。

## 唯一状态路径

1. allocator 继续唯一拥有 total/free frame 状态，TaskManager process graph 唯一拥有 live thread 集合与 load EWMA，timer 唯一提供 monotonic uptime。
2. task façade 的 `SystemInfoSnapshot` 与 `ProcSource` 共用 `process_snapshot()` 采集边界；快照只投影状态，不缓存、不加锁、不拥有 counter。
3. syscall 层只把千分制 load 转成 `SI_LOAD_SHIFT=16`，把 uptime 向上取整到秒，并编码 RV64 UAPI。当前没有 swap/highmem/page-cache owner，因此对应字段诚实返回零，RAM 使用 byte 值和 `mem_unit=1`。

## 验收契约

- `ps` 同时观察到 `init` 与交互 shell。
- `free` 的 total 与 `/proc/meminfo` 的 `MemTotal` 一致，证明两个 consumer 来自同一 allocator 状态。
- `uptime` 输出 Linux load-average 形状。
- 动态 BusyBox gate 出现 `LITEOS_OBSERVABILITY_42`，且启动日志不出现 unsupported syscall。
- `make verify` 完整通过。
