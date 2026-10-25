# device-manager

- **类型**：程序（可执行文件）
- **作用**：设备管理器：按类别列出系统里的硬件
- **产物 / 安装位置**：`/bin/devmgmt`
- **对应的 Windows 2000 组件**：devmgmt.msc（MMC 管理单元）
- **依赖**：classic-ui、classic-theme、canvas、typeface、linux-uapi、liteos-runtime

类别树（处理器、磁盘驱动器、显示卡、声音和游戏控制器、键盘、鼠标和其他指针设备、网络适配器），
双击设备或"操作→属性"查看详情。数据来自 `/proc/stat`、`/sys/block`、`/dev/dri`、`/dev/snd`、`/dev/input`
（经 evdev 取设备名）与 `/proc/net/dev`；内核没有统一的设备枚举接口，所以这里只列出这些来源能确认的设备。
只读：没有停用、卸载、更新驱动程序。控制面板"系统→硬件"的"设备管理器"按钮启动它。
