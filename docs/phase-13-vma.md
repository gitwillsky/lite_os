# Phase 13：统一 VMA 与 anonymous private mapping

## 目标与边界

本阶段建立地址空间映射的唯一 owner，并接入 Linux/riscv64 `munmap(215)`、`mmap(222)`、`mprotect(226)`。首个竖切只覆盖 eager anonymous private mapping；不以占位接口暗示 file/shared/lazy/COW 已存在。

## 所有权与不变量

- `MemorySet::areas` 是按起始 VPN 排序的唯一 VMA 表，同时拥有 framed mapping 的 `FrameTracker`。原独立 `UserHeap` shadow state 已删除，program break 与 limit 存放在 heap VMA 内。
- VMA 区间不得重叠；所有 ELF、stack、heap、trap context、kernel mapping 与 anonymous mapping 都经同一插入入口验证。
- `mmap` 先完成参数、地址和冲突验证，再 eager 分配；中途 OOM 会解除本次已经建立的 PTE 并释放 frame，VMA 表不提交半成品。
- `munmap` 修改前验证所有相交 VMA；anonymous VMA 可拆为左右残片，未映射洞被忽略，非 anonymous VMA 不被隐式破坏。
- `mprotect` 先证明整个区间由 anonymous VMA 连续覆盖，再更新 PTE；边界产生 VMA split，权限相同的相邻 anonymous VMA 随后合并。
- 所有成功页表变更执行本地 `sfence.vma` 与同步 SBI RFENCE；用户 W+X 始终拒绝。

## ABI 范围

- `mmap`：只接受 `MAP_PRIVATE|MAP_ANONYMOUS`，可选 `MAP_FIXED_NOREPLACE`；anonymous fd 必须为 -1、offset 必须为 0。
- `munmap`：地址必须页对齐，length 非零并向上取整；只允许解除 anonymous mapping。
- `mprotect`：地址必须页对齐，完整区间必须已映射为 anonymous private；支持 R、RW、X，禁止 W+X。
- 当前不支持 `PROT_NONE`、destructive `MAP_FIXED`、file/shared mapping、COW 与 lazy fault，支持矩阵明确标记为 Partial。

## 启动验收

`/bin/init` 建立三页 RW mapping，覆盖首/中/尾页写入、单页 R→RW 权限变更、中间页解除与 fixed-noreplace 重新映射、冲突 `EEXIST`、W+X `EACCES`、新页清零和跨多个 VMA 解除。`scripts/verify_boot.py` 在 `-smp 1/3/8` 冷启动中要求 `vma ok`。

## 验证结果

`make verify` 已通过：格式、RISC-V workspace check、Clippy `-D warnings`、架构/接口围栏、三组件构建、ELF 静态检查、`git diff --check`，以及 QEMU `-smp 1/3/8` 冷启动均成功。
