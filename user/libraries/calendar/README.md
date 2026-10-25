# calendar

类型：库，内部系统产物 `libcalendar.so`，SDK 应用可静态纳入自己的构建。

拥有公历算法、标准 TZif v1/v2/v3 的解析/投影、原子持久化与并发安全重载；固定依赖 tz-rs 0.7.3。
不修改进程环境，解析错误保留旧模型；系统时钟写入通过 linux-uapi 的标准 CLOCK_REALTIME。
规范与支持边界见 [规范基线](../../../docs/standards-baseline.md) 和 [UI 架构](../../../docs/architecture/ui-framework.md)。
