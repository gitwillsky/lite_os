# 崩溃诊断契约

## Owner

`crash` 独占 panic CAS、停止请求、CPU 快照、记录与 watchdog；只消费静态 arch/CPU/log/memory/platform/timer/VM 接口。
不依赖 devmgr、GPU 或 Linux procfs。boot 先 reserve，再把可分配上界交给 memory，页表仍映射完整 RAM。

| 状态 | owner | 缺失后果 |
|---|---|---|
| 记录保留区 | `crash::record` | allocator 覆盖现场或多个写者破坏校验 |
| panic/停止所有权 | `crash::panic` | 多 CPU 交错写入，其他 CPU 在失效不变量下继续运行 |
| 预分配 CPU 快照 | `crash::panic` | panic 路径无法在禁分配条件下保存现场 |
| reboot 策略 | `crash::panic` | 无人值守反复重启且无法读现场 |
| deferred 进展 | `crash::watchdog` | 有 tick 的工作停滞表现为静默冻结 |
| 上次记录只读副本 | `crash::PREVIOUS` | recover 作废保留区后无内容可供系统服务读取 |

## 安全与退休

- CAS 胜者是 recorder 唯一写者；boot recover 在 SMP 启动前读取并作废。commit 只校验已经初始化的文本。
- 停止 IPI 在 ack/EOI 与同步 barrier completion 后检查；Release/Acquire 确认有界等待，超时不能宣称 CPU 停止。
- 回溯只读取真实 SP 所属的存活 kernel/boot/secondary stack 范围，检查完整 frame 和地址溢出，不分配或锁页表。
- log tail 只 try_lock；锁忙放弃该项，禁止 force-unlock 或制造第二个 guard。
- native `CRASH_LOG` 只复制 immutable 摘要；linuxd 的 `/proc/last_panic` 投影不访问内核 Rust 全局状态。
- watchdog 只在 boot tick 检查内核 deferred 进展；不替服务队列维护第二份进度。

正常路径只增加既有停止标志/时钟读取；记录与回溯仅在 fail-stop 执行，不设宿主性能阈值。
没有 GPU panic 绘制或设备寄存器轮询路径；整机 IRQ 屏蔽死锁不在软件 watchdog 的检测范围。
