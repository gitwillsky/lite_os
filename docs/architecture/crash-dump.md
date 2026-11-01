# 内核崩溃与现场保存

owner 为 `kernel/src/crash/`，panic handler 只转交它。

1. panic 关本地 IRQ，以 CAS 确立唯一写者；嵌套或竞争失败者停机。
2. 对其余 online CPU 发停止 IPI，有界等待现场；无响应不视为已停止。
3. 写 panic 原因、回溯、CPU 快照和日志尾部，并镜像到串口。
4. 按 `panic=N` 停机或 warm reset：0 停机，正数延迟，负数立即重启。

RAM 顶端保留记录区在 frame allocator 发布前排除，包含版本、长度和校验。下一次 boot 校验、复制后作废原区。
内核经 native system-info `CRASH_LOG` 提供只读摘要；linuxd 缓存读取结果并经 `/proc/last_panic` 呈现。
当前内核无 GPU/scanout owner，不绘制蓝屏。设备/服务失败与内核 panic 的监督路径不同。

boot CPU 的硬中断 tick 检查内核 deferred 进展；有 tick 而工作长时间不前进触发软 watchdog。
全部 CPU 屏蔽 IRQ 的死锁无法由该机制发现。记录只跨 warm reset 存活，宿主销毁 VM 会丢失 RAM；它是文本摘要，不是完整内存转储。

安全与清理见[崩溃契约](../architecture-contract/crash.md)。
