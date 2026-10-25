# classic-ui-abi

类型：库，公开 UI SDK 的稳定二进制布局声明（no_std，不安装独立共享库）。

拥有版本化函数表、标量结构、状态与事件标签，不拥有控件或协议状态。
运行库分配对象、配对释放，调用期间复制字符串；Rust 容器、Future 与回调对象不跨边界。
调用约定、内存与兼容政策见 [UI 契约](../../../docs/architecture-contract/ui-framework.md)。
