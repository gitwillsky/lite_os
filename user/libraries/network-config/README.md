# network-config

- **类型**：库
- **作用**：系统网络接口配置 `/etc/network/interfaces`（interfaces(5) 子集）的读写与 network-service 重载
- **产物 / 安装位置**：静态链接进使用它的程序（资源管理器、控制面板）
- **对应的 Windows 2000 组件**：netcfgx 的 TCP/IP 设置与 netshell 的连接启用/禁用
- **依赖**：linux-uapi

控制面板的“Internet 协议 (TCP/IP) 属性”与资源管理器的“启用/禁用”通过它写回配置：同目录独占临时文件、sync 后改名，
再向 `/run/network-service.pid` 记录的服务发 TERM，由 init 以新配置重启服务。配置里没有的以太网接口按 Windows 默认：
启用、自动获得 IP 与 DNS；不认识的节内选项原样保留。
