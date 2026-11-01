# 微内核读码地图

事实与约束以[文档索引](README.md)中的 owner 为准；本页只提供阅读顺序，路径从仓库根计算。

## 从机制进入服务

1. `kernel/src/main.rs` → `execution/entry` → `arch/mod.rs`：raw handoff 如何转为 typed 输入、静态后端如何装配。
2. `kernel/src/execution/trap.rs` → `thread/user_context.rs` → `arch/<target>/trap.S`：一次异常如何到达 native owner，以及最终 IRQ-closed 返回窗口。
3. `kernel/src/memory/frame_allocator.rs` → `vm/page_table.rs` → `vm/translation.rs`：物理页、页表机制与“先撤销翻译，再退休 backing”。
4. `kernel/src/vm/vmo.rs` → `vm/address_space.rs`：COW hidden parent、页可见性、VMO/VMAR 与 pager 请求。思考：缩短后增长为何不能再次暴露祖先页？
5. `kernel/src/thread/scheduling.rs` → `processor/ready_membership.rs` → `context_switch.rs`：线性 token 如何连接 run state、per-CPU projection 和保存后的 handoff。
6. `kernel/src/object` → `syscall` → `bootstrap`：句柄、channel/port/event、resource 与 restricted mode；这里没有 Linux syscall 表。
7. `system/root-server/src/main.rs` → `system/linuxd/src/main.rs`：服务 capability 分配和 Linux 环境建立；应用从 restricted mode 返回服务处理 syscall。
8. `system/linuxd/src/memory/mm` → `fs/pager.rs` → `fs/page_cache/fill.rs`：Linux VMA 政策如何使用 native 对象，pager 如何跨服务读取文件。
9. `system/devmgr/src/platform` → `drivers` → `system/ext4d/src/ext4` → `system/netd/src/inet`：设备、磁盘格式、网络协议各有独立 owner，通过协议 crate 连接。

## 配合验证阅读

`tools/kernel-unit` 直接装配实际 native/Linuxd/设备叶模块，`tools/ext4-unit` 与 `tools/netd-unit` 分别执行实际服务引擎。
`tools/scheduler-unit` 执行实际调度状态；host 只替换 CPU/IRQ/frame/NIC 等边界，不实现第二套领域算法。
`fallible-tree` 可单独读结构化节点迁移与 OOM publication；`scripts/fixtures/musl` 展示应用可见 ABI 的真实调用。

host 通过只能证明该装配下的状态和寿命；硬件 TLB、跨 CPU 调度、DMA、服务 RPC 与应用兼容性必须继续看对应 guest gate。
