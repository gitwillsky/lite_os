# desktop

- **类型**：程序（可执行文件）
- **作用**：用户桌面外壳
- **产物 / 安装位置**：`/bin/desktop`
- **对应的 Windows 2000 组件**：explorer.exe 的外壳部分
- **依赖**：classic-ui、classic-theme、canvas、typeface、linux-uapi、sound、sndvol、desktop-entry

桌面背景色与墙纸（与显示属性预览共用 classic-ui 合成规则，墙纸只在路径变化时解码、按图层尺寸合成后缓存）、桌面图标（含 `~/Desktop`）、任务栏、开始菜单、托盘（时钟、音量面板）、运行对话框与右键菜单。任务管理器、音量控制、控制面板、资源管理器都是独立程序，外壳只负责启动它们。
