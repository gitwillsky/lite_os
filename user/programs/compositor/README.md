# compositor

- **类型**：程序（可执行文件）
- **作用**：显示服务与窗口管理器
- **产物 / 安装位置**：`/bin/compositor`
- **对应的 Windows 2000 组件**：Windows 2000 的 win32k 窗口管理与 GDI 合成
- **依赖**：canvas、classic-theme、classic-ui、typeface、keyboard、linux-uapi、wayland-wire、boot-scene

唯一的 DRM master 与 evdev 读者；Wayland 服务端、窗口栈、焦点、光标、整帧 CPU 合成、启动画面交接与 Ctrl+Alt+Delete 安全注意序列，以及 Alt+Tab 切换面板（`switcher.rs`，借用 classic-ui 的 Painter 与 typeface 绘制）。`src/bin/` 下的 `wayland-probe`、`shell-probe` 只用于运行时门禁。
