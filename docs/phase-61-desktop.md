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
- architecture checker 与 Linux-musl rootfs build 已通过；仓库规则禁止执行测试用例，因此 milestone
  中 pointer latency、连续 resize、idle CPU、RSS 与 crash recovery 仍需后续显式运行时测量，不能以
  build 成功冒充性能门通过。
