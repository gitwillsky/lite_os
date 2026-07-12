# Phase 42：墙钟时间可观测性竖切

## 目标

以 timer module 的唯一 RTC+monotonic realtime owner 实现 Linux `gettimeofday(169)`，并让固定动态 BusyBox `date` 成为墙钟 consumer。禁止复制 realtime offset、引入无 setter 的 timezone state，或用 libc wrapper 假装验证 syscall 169。

## 固定规范

- Linux `v7.1` commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`：`include/uapi/linux/time_types.h`、`include/uapi/asm-generic/unistd.h` 与 `kernel/time/time.c::gettimeofday`。
- musl `v1.2.6` commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`：`src/time/gettimeofday.c`；公开 wrapper 复用 `clock_gettime(CLOCK_REALTIME)`。
- BusyBox `1.37.0`：`coreutils/date.c`。

RV64 timeval 为两个 64-bit long，共 16 bytes；legacy timezone 为两个 i32，共 8 bytes。Linux 允许两个指针独立为空，并按 timeval→timezone 顺序 copyout。

## 唯一状态路径与验收

`gettimeofday` 与 `clock_gettime(CLOCK_REALTIME)` 共用 `timer::get_realtime_ns()`，只在 ABI 边界把 nanoseconds 截断为 microseconds。当前无 settimeofday，timezone policy 固定 UTC `{0,0}`。动态 musl probe 通过 raw `syscall(SYS_gettimeofday)` 对比 clock_gettime 并验证 timezone；BusyBox gate 以 `date -u +%s/%Y` 验证合理 epoch，要求 `LITEOS_WALLCLOCK_42`。
