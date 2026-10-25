# liteos-ui

类型：Rust 源码 SDK 库，随应用编译，只依赖 classic-ui-abi。

提供安全 Window/Widget、类型化事件、RAII 任务、非 Send Future、定时/FD、后台工作与通用对话框等待。
控件交互/绘制在版本化共享运行库中实现，SDK 不静态复制控件；不要求与 OS 使用同一 Rust 工具链。
使用方式见 [SDK 开发](../../../docs/development/ui-sdk.md)，当前 ABI 仍待兼容性验收。
