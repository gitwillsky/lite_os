# Phase 61: QuickJS/Solid desktop

## Goal

把现有 direct DRM terminal/reference client 收敛为一个 compositor-owned desktop session，并以
QuickJS/SolidJS 应用、Rust 集中式 LiteUI core 和 Alpine APK profile 完成第一条可用桌面竖切。

## Implementation order

1. 建立 `liteui-core` 与 transaction/tree/style/layout/draw/raster deep interfaces。
2. 将 `liteos-2d` 迁移为唯一 `liteui-compositor`；保留已证明的双 buffer、presenter、hotplug、
   resize 与 damage lifecycle，删除 demo scene identity。
3. 建立固定 QuickJS build/cache、`liteui-host` 与 Solid universal renderer bundle。
4. 建立 `liteui-session`，将图形 session 从 init 的多个 respawn action 收敛为一个 generation owner。
5. 交付 System Shell APK、Terminal service/TextGrid 与 Calculator APK。
6. 更新 rootfs/APK builder、standards baseline、architecture checker 和当前事实文档。

## Exit criteria

[desktop/milestone-1.md](desktop/milestone-1.md) 的 required vertical slice 与 performance gates 全部
通过；rootfs 不再包含 `/bin/liteos-2d` 或 direct-DRM `/bin/liteos-terminal`，且 checker 对唯一
compositor、完整 crate/source 集、APK profile、禁止脚本与 QuickJS cache key 做精确比对。

## Current state

- 实现竖切已闭合：唯一 compositor、三 identity client slot、QuickJS/Solid System Shell、Calculator
  APK、terminal-service/PTY、failure-atomic TextGrid、窗口输入路由与 session supervision 已进入 rootfs。
- terminal grid 使用 `LUG1` 完整 snapshot，compositor event 使用固定 64×24-byte `LUE1` ring；
  keyboard backpressure 留在 evdev OFD，resize 由 compositor geometry 经 configure→TIOCSWINSZ 单轨提交。
- compositor 以 root-only、固定 160-byte、无动态分配的 `LUD1` snapshot 投影 pointer/damage/resize
  运行指标；诊断只读取 reactor 的标量投影，不复制 scene，也不成为第二状态 owner。
- `make verify-runtime-desktop` 已在真实 QEMU `virt -smp 8` 中通过：RFB pointer sweep 的可见延迟
  不超过 20 ms 且 damage 小于屏幕八分之一；持续 titlebar drag 只更新 outline preview，release 一次提交
  geometry；keyboard key-to-visible 不超过 50 ms；连续 `800×600 → 1024×768 → 1280×720` host
  resize 合并为一次 commit，session 与全部 client PID 保持不变；稳定 idle 两秒的六个桌面进程总增量
  不超过 4 ticks；各角色 RSS 低于 milestone gate；Shell 与 compositor `SIGKILL` 后均按 generation
  契约恢复。
- runtime gate 消费基准 rootfs 的私有副本，并以完整 build/runtime 输入和 QEMU identity 做内容寻址缓存；
  因此日常迭代可命中已验证结果，执行过的路径也不会被误写成对任意负载或真实硬件的外推证明。
