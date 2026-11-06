# desktop-entry

- **类型**：库
- **作用**：桌面条目解析、文件关联与外壳文件操作
- **产物 / 安装位置**：共享库 `libdesktop_entry.so`
- **对应的 Windows 2000 组件**：shell32.dll 的程序、文件关联与文件操作
- **依赖**：liteos-runtime、linux-uapi、calendar

freedesktop Desktop Entry 子集：名称、图标、分类与命令，Exec 不经过 shell。
文件复制、移动、回收与撤消共用 `fileops`，调用方在后台执行并负责呈现错误与更新界面。
