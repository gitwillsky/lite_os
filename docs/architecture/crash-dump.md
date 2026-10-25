# 内核崩溃：现场保存与蓝屏

owner 模块：`kernel/src/crash/`。`lang_item.rs` 的 `#[panic_handler]` 只转交 `crash::handle_panic`。

## 流程

1. panic 关本地中断并争夺所有权（CAS）；失败者与嵌套 panic 直接停机。
2. 向其余在线 CPU 发停止 IPI（既有 platform `send_ipi`），目标在核间中断 handler 中采集 fp 回溯并停机；最多等 200 ms，不响应的 CPU 记为“无响应”。
3. 依次写入 panic 原因 + 回溯、各 CPU 回溯、kmsg 尾部（128 条）；同时镜像到串口。每段写完提交一次。
4. 在 panic 专用 scanout resource 上绘制蓝屏（Windows 2000 风格），与 DRM 是否接管无关。
5. 按 `panic=N`（Linux 语义）停机或重启：默认 0，停在蓝屏；正数 N 秒后 warm reset；负数立即 warm reset。

## 崩溃记录区

RAM 顶端 512 KiB，在 `memory::init` 中于 frame allocator 之前划出并排除在分配之外，仍在 direct map。布局 `[Header 32B][UTF-8 文本]`，Header 含魔数、版本、长度与 FNV-1a 校验。下一次启动 `crash::recover` 校验后拷贝并作废记录，经 `/proc/last_panic` 提供（无记录时为空文件）。回溯地址用 `addr2line -e <kernel ELF>` 解析。

## 看门狗

boot CPU 硬中断 tick 仍到达、但其 deferred TIMER work 超过 30 s 未被消费时 panic。所有 CPU 同时屏蔽 IRQ 的死锁无法被软件发现（需要 pseudo-NMI 或硬件 watchdog），属当前限制。

## 当前限制

- 记录只跨 warm reset 存活；宿主销毁虚拟机进程会丢失 RAM。
- 记录是文本摘要，不是完整内存转储；没有寄存器全集与进程/页表视图。
- 蓝屏只含 ASCII。

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
| `VirtIOGpuDevice::panic_screen` | `virtio::gpu` | DRM 接管后 panic 无法上屏 |

unsafe：`Recorder::begin`（单写者）、`IrqMutex::lock_for_fail_stop`（其余 CPU 已停止）、`arch::backtrace::read_frame`（调用者已验证栈窗口）、`PanicFramebuffer::present` 的强行解锁（同上）。
