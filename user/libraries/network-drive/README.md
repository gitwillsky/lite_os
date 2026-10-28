# network-drive

- **类型**：库
- **作用**：每用户 SMB 映射记录、私有 credentials 文件与标准 fusermount3/smbfs 生命周期的唯一 owner
- **产物 / 安装位置**：静态链接进使用它的程序（资源管理器、桌面、smbfs）
- **对应的 Windows 2000 组件**：mpr 的映射网络驱动器与登录重连
- **依赖**：linux-uapi

记录目录为 `~/.config/liteos/network-drives/`，挂载目录为 `~/Network/<盘符>`；目录 0700，凭据文件 0600，
同目录原子替换，跨窗口用标准 flock 串行化。认证仅经 stdin 传递，密码不进入命令行与环境变量。
普通用户经上游 setuid fusermount3 挂载，不提供 root 代挂服务；SMB 协议与 FUSE 翻译属于 smbfs。
凭据采用 mount.cifs 文件格式，以文件权限保护，root 与用户备份仍可读取；未接入系统密钥环。
