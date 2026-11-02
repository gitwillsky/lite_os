# network-config

- **类型**：库
- **作用**：原生 netcfg 网络设置的 Linux 客户端（经 `/dev/network-settings`，需要 netdev 组）与 sysfs 链路快照
- **产物 / 安装位置**：静态链接进使用它的程序（资源管理器、控制面板、设备管理器、resolvd）
- **对应的 Windows 2000 组件**：netcfgx 的 TCP/IP 设置与 netshell 的连接启用/禁用
- **依赖**：linux-uapi、netcfg-protocol

控制面板的“Internet 协议 (TCP/IP) 属性”与资源管理器的“启用/禁用”通过它把设置提交给 netcfg：netcfg 先持久化再重新
配置接口，Linux 侧没有配置文件或需要重载的服务。没有单独设置的以太网接口按 Windows 默认：启用、自动获得 IP 与 DNS；
禁用时 netcfg 保留之前的静态地址，重新启用后恢复。
