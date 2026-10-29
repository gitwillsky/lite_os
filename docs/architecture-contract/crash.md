# 崩溃诊断契约

## Owner 与依赖

`crash` 拥有 panic 所有权、停止请求、快照、记录内容与 watchdog 策略；只依赖 arch、CPU、
drivers、log、memory、platform 与 timer 的窄接口。boot 装配调用 `reserve_region` 后把
allocator 上界交给 `memory::init`；memory 不反向调用 crash，页表仍映射完整 RAM。
启动先按已发布 logical topology 初始化快照，再恢复记录、登记 typed `PanicFramebuffer` 与配置 `panic=`。

## Owner 与失败后果

| 状态 | owner | 缺失后果 |
| --- | --- | --- |
| `record::REGION` | `crash::record` | 无法保留现场；单写者规则缺失会使两个 CPU 的文本交错、校验失败 |
| `panic::OWNER` / `STOP_REQUESTED` | `crash::panic` | 多 CPU 同时 panic 交错输出；其余 CPU 在不变量失效后继续运行 |
| `panic::SNAPSHOTS` | `crash::panic` | panic 路径不能分配，各 CPU 位置无处存放 |
| `panic::REBOOT_SECONDS` | `crash::panic` | 故障机器无人值守反复重启、不留可读现场 |
| `panic::DISPLAY` | `crash::panic` | 无蓝屏，只有串口 |
| `watchdog::BOOT_CPU_DEFERRED_US` | `crash::watchdog` | deferred 死锁表现为静默冻结 |
| `PREVIOUS` | `crash` | 记录读出即作废，`/proc/last_panic` 无内容 |
| `VirtIOGpuDevice::panic_screen` | `drivers::virtio::gpu` | DRM 接管后 panic 无法上屏 |


## Interface 与安全证明

- `handle_panic` 只由 panic handler 调用，CAS 确立唯一写者；嵌套 panic 直接停机。
- `stop_if_requested` 在核间中断 ack/EOI 与同步内存屏障 completion 之后执行，不消费普通 deferred work。
  ack 采用 release/acquire；等待有界，无响应不能被解释为 CPU 已停止。
- 回溯只读取真实 SP 所属存活栈的完整 frame 窗口；memory 提供排除 guard 的 task 栈、boot 栈或
  已发布 secondary 栈范围，arch 提供 SP/FP 与 frame-record 读取。地址加法检查溢出，不锁页表或分配。
- `Recorder::begin` 的唯一调用者已赢得 panic CAS，保留区排除在 allocator 之外；boot recover 在 SMP
  启动前读取并作废，之后 panic 是唯一写者。每次 commit 校验已写入的固定区间，不读未初始化字节。
- log tail 仅 `IrqMutex::try_lock`，GPU fail-stop 的每次命令访问仅 `try_lock`；锁忙或设备超时则放弃该项。
  不 force-unlock，不与仍可能运行的 holder 建立第二份可写 guard。
- `/proc/last_panic` 通过 `ProcSource::previous_panic` 读取只读快照，只有 kernel source adapter 访问 crash；
  procfs 不跨过 source seam，不直接持有诊断可写状态。
- `watchdog_tick` 只在 boot CPU tick 检查既有 deferred TIMER 进展时间；正常路径只读 Atomic 与单调时钟。
  停滞触发 panic，不改变普通 timer/deferred 所有权。没有 tick 的全 IRQ 屏蔽死锁不在检测范围内。

## 成本与清理

正常 IPI 增加一个停止标志的 acquire 读取，watchdog 仅 boot tick 检查；不增加分配、锁或普通 deferred dispatch。
回溯、快照输出与 GPU 有界轮询只在 fail-stop 路径，不设宿主墙钟阈值。快照按 topology 一次分配，
启动记录只在恢复时拷贝；未发布状态以无记录/无显示处理，不伪造成功或等待不存在的 owner。
