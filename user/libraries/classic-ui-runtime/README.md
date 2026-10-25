# classic-ui-runtime

类型：共享库，产物 `libliteos_ui.so.1`，唯一动态入口 `liteos_ui_get_api`。

拥有控件树、布局、交互、通用对话框与标准 Wayland 客户端的装配；自身静态纳入 Rust 依赖和 std。
稳定 ABI 按 context/window/widget/dialog 分开编码，领域状态留在对应模块。
系统应用与外部 Rust SDK 消费同一实现；事实、边界与尚未验收项见 [UI 架构](../../../docs/architecture/ui-framework.md)。
