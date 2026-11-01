# 启动与平台契约

## Owner 与接口

- `arch` 独占 raw register/context/MMU 编码；`execution::entry` 只构造 typed boot/trap 输入。AArch64 只接受 Image/x0 DTB/PSCI，RISC-V 只接受独立 bootloader 的 handoff/DTB/SBI。
- `platform` 发布 immutable RAM/CPU/interrupt/serial facts；hardware identity 在 `execution::cpu` 投影为 logical identity。通用领域不得引用 concrete platform/arch adapter。
- kernel platform DTB decoder 只解析内核启动所需事实；devmgr 的 DTB decoder 独占通用设备、PCI 与 IOMMU 装配。各 decoder 按 compatible 判断设备身份，不依赖任意节点标签。
- `mmio::MmioBus` 验证非空、地址溢出、子窗口包含、每次访问边界和对齐。寄存器实际访问形态只属于静态 ISA backend；adapter 不使用裸 volatile 或自选 target 指令。
- GIC claim/EOI 是线性 token；PLIC source 编码拒绝 0 哨兵与越过 enable bitmap 的编号。external hardirq 单次有界处理，不为探测 backlog 提前消费下一 claim。
- RISC-V RFENCE sender 在 request Release 前写 start/size，target 在 Acquire 后读取，执行范围 fence 后 Release ack。sender 等待 Acquire completion 后才允许 backing 退休。

## 启动所有权

1. 内核 `main` 在发布 secondary-ready 前完成全局对象和 root-server 装配；secondary 只消费已发布状态。
2. kernel bootstrap loader 只载入 root-server；root-server 唯一装配系统服务连接和初始 capability。
3. devmgr 独占设备权限；linuxd 通过设备/文件系统/网络服务初始化 Linux 环境并执行 init。服务间不共享 Rust 全局状态。

DTB、initrd 和崩溃保留区必须先从 frame allocator 排除；缺失时 bootstrap 尚未复制的输入可能被覆盖。
设备 IRQ thread 只确认设备并请求完成分派；可以阻塞的队列事务在服务线程中执行，不允许恢复启动期 MMIO polling/WFI 双轨。
服务失败由启动监督者处理，不以伪造 ready marker 或无设备假成功继续启动。

## 失败与验证

缺失 CPU、RAM、firmware、中断控制器或必需串口事实时 fail-stop；不猜测 machine 地址。
设备初始化、MMIO/IRQ/BTI 失败由 devmgr 拒绝相应设备，不能伪造 DMA 隔离或驱动能力。
host DTB/MMIO fixture 验证解码与边界；release ELF 检查固定 ISA façade；双架构冷启动才验证真实装配与中断活性。

root-server 的 role 句柄交付保留最小设备边界：devmgr 接收根 resource；linuxd 接收可再细分的 system
操作范围（日志读取、串口、UTC 设置、电源），ext4d/netd 只接收其服务和设备连接。
resource::authorization 是 immutable kind/range 的唯一 owner；同类子范围不能扩大，hardware/system 不互相转换。
