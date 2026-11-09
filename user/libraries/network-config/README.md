# network-config

- **类型**：库
- **作用**：原生 router-config 网络状态与设置的 Linux 客户端，以及 sysfs 链路快照。普通用户查询 `/dev/network-status`；设置与订阅经 `/dev/network-settings`，需要 netdev 组。
- **产物 / 安装位置**：静态链接进使用它的程序（资源管理器、控制面板、设备管理器、resolvd）
- **对应的 Windows 2000 组件**：netcfgx 的 TCP/IP 设置与 netshell 的连接管理
- **依赖**：linux-uapi、router-protocol

控制面板的“Internet 协议 (TCP/IP) 属性”与资源管理器的“启用/禁用”通过它把设置提交给 router-config：router-config 先暂存可逆执行、
持久化后确认；失败撤销，确认未完成会明确返回错误。Linux 侧没有配置文件或需要重载的服务。没有单独设置的以太网接口按 Windows 默认：启用、自动获得 IP 与 DNS；
禁用时 router-config 保留之前的静态地址，重新启用后恢复。

连接列表、状态和属性窗口、任务栏共用 `ethernet_links()` 的显示名。重命名持久化在 router-config，实际操作接口时继续使用
链路快照中的 Linux 接口名。

客户端保留完整原生 DNS 与搜索列表；仅 resolvd 在生成 musl 配置时应用解析器容量。持久配置加载失败的 Rejected 状态不允许局部更新覆盖未知配置。

`configuration::{export,schema,validate,difference,import}` 经同一 netdev 授权端点操作原生配置树：导出不含租约，
校验没有副作用，对比仅供审阅，完整导入带预期快照代际并复用整批提交。Rejected 时可从状态快照/校验取得代际，再整份导入恢复。
代际冲突需要重新查询；修改超时或断线也不能盲目重放。正文通过标准 SCM_RIGHTS fd，控制帧与正文大小解耦；协议、所有权与未完成验收边界见网络契约。

`configuration::{backups,create_backup,export_backup,delete_backup,restore_backup}` 提供备份管理；自动策略属于原生配置树的 backup 字段。
手动副本不受自动轮转删除，恢复仍走带预期代际的完整导入。查询返回真实调度状态/存储元数据；修改结果不确定时先刷新列表。
