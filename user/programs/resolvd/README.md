# resolvd

- **类型**：程序（可执行文件，init 监督，root 运行）
- **作用**：依据原生 router-config 的网络设置快照维护 `/etc/resolv.conf`，接收手工、租约和有效的 IPv6 通告 DNS
- **产物 / 安装位置**：`/bin/resolvd`
- **对应的 Windows 2000 组件**：无（Linux 侧的 DNS 配置代理，相当于发行版的 resolvconf）
- **依赖**：network-config
