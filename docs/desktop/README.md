# LiteOS desktop design index

本目录拆分桌面设计，避免把 runtime、wire protocol、package policy 与 milestone 混入同一巨型
文档。稳定 owner/interface 只在
[architecture-contract/desktop.md](../architecture-contract/desktop.md) 定义；这里解释设计与演进。

| 文档 | 内容 |
|---|---|
| [runtime.md](runtime.md) | 进程拓扑、信任边界、启动与失败恢复 |
| [protocol.md](protocol.md) | Solid HostOps、有界 transaction、frame/input 时序 |
| [milestone-1.md](milestone-1.md) | 第一阶段功能、非目标、预算与验收 |

外部设计基线：PocketJS 只提供“JS adapter + 单 Rust UI core + backend-neutral DrawList”的架构
参考；LiteOS 不直接暴露或 fork `pocketjs-core`。语言引擎固定 QuickJS，layout mechanism 固定
Taffy；二者的精确版本、source digest 与配置进入 `standards-baseline.md` 后才允许进入 rootfs。
