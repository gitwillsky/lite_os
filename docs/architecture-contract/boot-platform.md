# 启动与平台契约

## Owner 与接口

- `arch` 独占 raw register/context/MMU 编码；`execution::entry` 只构造 typed boot/trap 输入。AArch64 只接受 Image/x0 DTB/PSCI，RISC-V 只接受独立 bootloader 的 handoff/DTB/SBI。
- `platform` 发布 immutable RAM/CPU/interrupt/serial facts；hardware identity 在 `execution::cpu` 投影为 logical identity。通用领域不得引用 concrete platform/arch adapter。
- kernel platform DTB decoder 只解析内核启动所需事实；driver-manager 的 DTB decoder 独占通用设备、PCI 与 IOMMU 事实及节点装配。各 decoder 按 compatible 判断设备身份，不依赖任意节点标签。
- 固件、kernel 与 driver-manager 共用唯一 vendored DTB walker。walker 保留地址/长度/中断 cell 上下文的同时输出原始属性；
  控制器绑定可据此校验自己的 cell 数。status 使用 walker 的 typed 属性，不把被内部消费或转为 typed 的属性当作缺省事实。
- `mmio::MmioBus` 验证非空、地址溢出、子窗口包含、每次访问边界和对齐。寄存器实际访问形态只属于静态 ISA backend；adapter 不使用裸 volatile 或自选 target 指令。
- GIC claim/EOI 是线性 token；IMSIC STOPEI claim 原子清除 EIP，0 是无消息哨兵；APLIC source 和 CPU 本地 EIID 分别校验范围。external hardirq 单次有界处理，不为探测 backlog 提前消费下一 claim。
- RISC-V RFENCE sender 在 request Release 前写 start/size，target 在 Acquire 后读取，执行范围 fence 后 Release ack。sender 等待 Acquire completion 后才允许 backing 退休。

## 启动所有权

1. 内核 `main` 在发布 secondary-ready 前完成全局对象和 component-manager 装配；secondary 只消费已发布状态。
2. kernel bootstrap loader 只载入 component-manager；component-manager 唯一装配系统服务连接和初始 capability。
3. driver-manager 独占设备根权限，驱动宿主只持有所属节点的资源；linuxd 通过设备/文件系统/网络/音频服务初始化 Linux 环境
   并执行 init。服务间不共享 Rust 全局状态。

bootfs（cpio `newc`）的文件内容按 4 KiB 页对齐（打包器把填充放进路径名字段；读取器在第一个 NUL 处截断名称），
使 ELF 段与大文件直接成为 bootfs VMO 的写时复制子对象，装载与交付不复制字节；子对象大小向上取整到页，精确长度的小配置仍复制。
根服务的可信 bootfs 句柄持 READ/EXECUTE/MAP/DUPLICATE/TRANSFER/INSPECT、无 WRITE；ELF 子对象继承执行能力，
通用 VMAR 的映射 ceiling 检查保持不变。DTB 仅供读，不具执行能力。
客户端提供的替换映像同样复制，不与可被其改写的 VMO 共享页。
DTB、initrd 和崩溃保留区必须先从 frame allocator 排除；缺失时 bootstrap 尚未复制的输入可能被覆盖。
设备 IRQ thread 只确认设备并请求完成分派；可以阻塞的队列事务在服务线程中执行，不允许恢复启动期 MMIO polling/WFI 双轨。
服务失败由启动监督者处理，不以伪造 ready marker 或无设备假成功继续启动。

## 失败与验证

缺失 CPU、RAM、firmware、中断控制器或必需串口事实时 fail-stop；不猜测 machine 地址。
设备初始化、MMIO/IRQ/BTI 失败由驱动宿主拒绝相应设备，不能伪造 DMA 隔离或驱动能力。
host DTB/MMIO fixture 验证解码与边界；release ELF 检查固定 ISA façade；双架构冷启动才验证真实装配与中断活性。

component-manager 的 role 句柄交付保留最小设备边界：driver-manager 接收根 resource 并为每个节点切出 MMIO/IRQ resource
与 BTI；linuxd 接收可再细分的 system 操作范围（日志读取、串口、UTC 设置、电源），fshost 只接收块设备与 ext4 实例的 Job/映像，
ext4/netstack/audio-core 只接收其服务和设备连接。
resource::authorization 是 immutable kind/range 的唯一 owner；同类子范围不能扩大，hardware/system 不互相转换。
