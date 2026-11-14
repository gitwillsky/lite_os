# classic-ui-runtime

类型：共享库，产物 `libliteos_ui.so.1`，唯一动态入口 `liteos_ui_get_api`。

拥有控件树、布局、交互、通用对话框与标准 Wayland 客户端的装配；自身静态纳入 Rust 依赖和 std。
稳定 ABI 按 context/window/widget/dialog 分开编码，领域状态留在对应模块。
系统应用与外部 Rust SDK 消费同一实现；事实、边界与尚未验收项见 [UI 架构](../../../docs/architecture/ui-framework.md)。

## 多文档工作区

Tree 持有子窗口视口、叠放顺序和指针手势。前窗拦截后窗的指针命中，只有前窗参加工作区内的 Tab 顺序。
关闭请求沿已有控件 Activate 交给应用，应用结束相应订阅或保留草稿。ABI v1 新增控件标签，复用布局、可见性和选择接口。
移动和缩放只重排该子树。置前使子窗覆盖失效，标题栏状态参与提交基线。所有子窗共用主窗口的 Wayland 表面及缓冲。
定向重绘用例检查重叠窗口的像素一致性，来宾验收覆盖同时打开、移动、缩放、切换及关闭。
[Win32 MDI](https://learn.microsoft.com/en-us/windows/win32/winmsg/about-the-multiple-document-interface)
