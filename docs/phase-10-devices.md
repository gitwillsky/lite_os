# LiteOS Phase 10：设备、中断与 DMA

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `abc6df0`（Phase 0–9）
> 目标平台：QEMU `virt`、8 hart、VirtIO MMIO legacy block、PLIC、Goldfish RTC

## 1. 阶段结论

设备层已收缩为实际启动使用的五个对象：

1. DTB `BoardInfo` 提供 MMIO/IRQ 资源；
2. 唯一 PLIC 控制器；
3. 唯一 primary read-only block device；
4. 一个 VirtIO MMIO 寄存访问器和一个串行 virtqueue；
5. timer 直接拥有的唯一 Goldfish RTC。

不再存在通用 device/driver/resource/power/bus registry，也没有未接入的 PCI/platform/console/DMA façade。当前设备只服务内核启动，不暴露用户设备 ABI。

## 2. 改动前的问题

| 严重度 | 问题 | 实际后果 |
|---|---|---|
| Major | `GenericBlockDriver` 被注册，VirtIO block 却由 manager 直接构造 | driver bind/probe 状态机完全不在真实路径 |
| Major | timer 已拥有 RTC，device registry 又创建第二实例 | 同一硬件有两个内核对象，后者只用于统计 |
| Major | PCI/platform bus、power manager、resource manager、VirtIO console 无实例 | 数千行未运行抽象扩大 unsafe/并发审计面 |
| Critical | `enable_interrupt` 在 `set_affinity(CPU0)` 后重新启用所有 context | affinity 被静默撤销，IRQ 可进入任意 context |
| Critical | PLIC 把 context 0..7 当作 hart 0..7 | QEMU PLIC context 是 `M0,S0,M1,S1...`，claim/complete 可消费错误 privilege/hart 的 IRQ |
| Critical | external handler 一次遍历所有 context | 一个 hart 可代理其他 hart claim IRQ，破坏 affinity 和 handler 所有权 |
| Critical | VirtIO 超时后在设备仍持有栈 DMA 地址时返回 | 晚到 DMA 写入已复用栈，导致内核内存破坏 |
| Critical | invalid used-ring chain 可部分回收并继续 | free list 可循环、重复描述符或 `num_free > size` |
| Major | ext2 已只读，block/VirtIO 仍暴露 write/async/sync/statistics | 公共能力与唯一调用链不一致 |

## 3. 最终调用链与所有权

```text
DTB VirtIO range
  -> bounded MmioBus
  -> VirtIODevice (legacy registers)
  -> VirtIOBlockDevice
  -> Mutex<VirtQueue>
  -> contiguous FrameTracker-owned DMA pages
  -> primary Arc<dyn BlockDevice>
  -> read-only ext2

DTB PLIC range
  -> single IrqMutex<PlicInterruptController>
  -> vector 8 handler Arc<VirtIOBlockDevice>
  -> current hart S-mode context claim -> device ack -> PLIC complete

DTB RTC range
  -> timer-owned GoldfishRTCDevice
  -> volatile low/high time reads
```

- primary block slot 只允许一次注册，重复设备返回 `AlreadyRegistered`。
- virtqueue 连续页由 `FrameTracker` 独占至队列销毁；队列指针不逃逸该生命周期。
- `Mutex<VirtQueue>` 串行化 descriptor/free-list/avail/used 状态；当前同时最多一个 block request。
- request/status/data 可位于栈或 heap，但同步 read 在 device used-ring 完成前绝不返回。

## 4. PLIC 不变量

- QEMU `virt` supervisor context 映射为 `2 * hart + 1`；初始化用 DTB MMIO size 验证最后一个 context 窗口。
- priority 只在注册实际 vector 时写入，不扫描假定的 1024 个中断源。
- affinity map 是 enable 的唯一依据；`enable_interrupt` 不会再将 vector 开到所有 hart。
- external trap 只 claim 当前 hart 的 S-mode context；设备 ack 完成后再 PLIC complete。

## 5. VirtIO/DMA 不变量

- MMIO 访问在每次 volatile read/write 前检查区间、溢出和对齐。
- queue 大小必须可表示为 `u16` 且为 2 的幂；desc/avail/used 偏移全部位于连续帧内。
- descriptor 写入先于 avail index 的 release publication；used index 以 acquire 读取后才读 device 写入的 ring/data。
- used descriptor ID、NEXT index、回收数和 `num_free` 均检查不超过 queue size；损坏链 fail-stop，不带着破坏 free list 继续。
- 发布 DMA descriptor 后的 notify 失败以及 used-ring 损坏都 fail-stop；返回普通错误会错误结束 DMA buffer 生命期。
- 设备无响应时当前会永久等待。在实现 device reset、queue reset 和 DMA quiesce 证明前，不恢复伪超时。

## 6. 删除内容

- `virtio_console.rs`；
- HAL `device.rs`、`memory.rs`、`power.rs`、`resource.rs`；
- PCI/platform bus 与通用 `Bus` trait，只保留有界 MMIO；
- `GenericBlockDriver`、driver registry、device registry、设备统计/查询平面；
- 重复 RTC 实例、alarm/property/hotplug/power/resource façade；
- block write/async/sync/statistics 和 VirtIO write/config 草稿；
- virtqueue stats/health/debug/force-recycle 表面与未使用 flag/error 类型。

## 7. 验证结果

- `cargo check --workspace`：通过；kernel warning 从 Phase 9 的 258 降至 132，驱动相关源文件无 warning。
- `make build-user`、`make build-kernel`、`make build-bootloader`：全部通过。
- 两轮 8-hart QEMU 冷启动（boot hart 4/4）：RTC 读取、VirtIO block 初始化、queue DMA、ext2 挂载、init 创建/入队均成功；观察窗口内无 panic/fault。
- `git diff --check` 和生产树反向搜索：无旧 HAL module/registry/console/block-write 残留。
- 按仓库规则未执行、维护或修正测试用例。

## 8. 剩余风险

- PLIC context 映射目前明确针对 QEMU `virt`；DTB parser 尚未解析 `interrupts-extended`/context topology，不宣称支持其他平台。
- VirtIO 目前是 legacy MMIO、固定 feature=0、单队列同步读；不支持 modern transport、indirect/event index、reset 或多请求并发。
- DMA coherence 依赖 QEMU `virt` 的 coherent 内存模型；当前不支持需要 cache maintenance 的非一致硬件。
- PLIC 格式和 VirtIO 内存序尚未在真实硬件上验证。

## 9. Phase 11 计划

1. 重建当前全量 syscall 矩阵 → verify：每个编号、参数形状、结构体和 errno 与 Linux/riscv64 对照。
2. 逐个审计仍暴露的 handler 语义 → verify：状态只能是 Complete/Partial/Missing/Not Planned/Removed，Partial 列明缺失。
3. 删除仍以标准名称暴露的错误近似实现 → verify：未支持编号统一 ENOSYS。
4. 构建、静态 ABI 对照与双轮 8-hart QEMU 启动 → verify：无新 warning、panic或 fault。
