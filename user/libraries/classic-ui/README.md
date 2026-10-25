# classic-ui

- **类型**：库
- **作用**：经典控件与窗口
- **产物 / 安装位置**：共享库 `libclassic_ui.so`
- **对应的 Windows 2000 组件**：user32.dll + comctl32.dll
- **依赖**：canvas、classic-theme、typeface、wayland-wire、liteos-runtime、desktop-entry

`Painter` 绘制边框、按钮、编辑框、滚动条、菜单、窗口框架；Wayland 窗口与输入、foreign-toplevel 与 layer-shell 客户端。`src/bin/` 的 `ui-probe`、`session-probe` 只用于门禁。
