# desktop-settings

- **类型**：库
- **作用**：每用户的桌面设置：任务栏选项与桌面背景色
- **产物 / 安装位置**：共享库 `libdesktop_settings.so`
- **对应的 Windows 2000 组件**：注册表 `HKCU\Control Panel\Desktop` 与任务栏设置
- **依赖**：liteos-runtime

`$HOME/.config/liteos/desktop.conf` 的读写：控制面板的"任务栏属性"和"显示属性"写它，桌面外壳按修改时间发现变化后应用。
文件是 `键=值` 的文本，缺失或损坏的项用默认值，不会阻止外壳启动。
