# Phase 18：固定 musl pthread create/join

> 本文保留 Phase 18 历史边界；mutex/condition/timedwait 后续结论见 [Phase 19](phase-19-musl-pthread-sync.md)。

## 实际调用链

固定 consumer 继续使用 musl `v1.2.6` commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`，不修改 libc。`user/musl-smoke.c` 在原静态启动、heap、clock 与输出路径上增加一个 joinable Thread：

`pthread_create -> mmap(PROT_NONE) -> mprotect(RW) -> clone -> child TLS/start -> pthread_exit -> private futex wake -> pthread_join -> munmap`

musl clone flags 为 `VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|PARENT_SETTID|CHILD_CLEARTID|DETACHED`。parent TID、child TLS、clear-child-tid 和 join futex 均沿现有 Process/Thread/TaskManager owner 工作，没有新增 libc patch、私有 syscall 或 thread join 内核入口。

## 已确认并修复的问题

1. anonymous `mmap/mprotect` 拒绝 `PROT_NONE`，导致 musl 默认 guard stack 在 clone 前返回 `EAGAIN`。
2. clone flags 拒绝 Linux 已保留为 ignored 的历史 `CLONE_DETACHED`，而 musl pthread 固定携带该 bit。

`PROT_NONE` 不能表示为 `V|U` leaf：RISC-V 会把 `V=1,R/W/X=0` 解释为下一级页表。终态模型由 anonymous VMA 的 `data_frames` 继续唯一持有 eager frame，页表只预留空 leaf slot；`mprotect` 在同一 frame 上 map/unmap leaf。fork clone、VMA partition/merge、munmap 不建立第二份 frame owner。

`CLONE_DETACHED` 只加入当前 thread clone 的合法 flag mask并保持无状态语义，与 Linux 当前行为一致；不增加 deprecated 分支或第二套 clone 路径。

## 验收边界

consumer 验证 child `pthread_self()`、参数/返回值、join completion 与资源回收，成功输出 `LiteOS musl pthread ok`。该证明只覆盖一个 joinable Thread，不包含 detached thread、program `PT_TLS`、mutex/condition timeout、cancellation、signal interruption、PI/requeue futex、thread-group-wide exit/exec 或动态链接。

最终执行 `make verify`：架构 fence、workspace check/clippy、三组件构建、ELF 静态检查、Rust init QEMU `-smp 1/3/8` 和固定 musl pthread consumer 冷启动全部通过。
