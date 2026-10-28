# smbfs

类型：程序，产物 `/bin/smbfs`；依赖 network-drive、linux-uapi、固定 smb-rs 与 fuser。

`serve //server[:port]/share mountpoint sign|encrypt` 从 stdin 接收 mount.cifs 格式凭据，
认证并处理 FUSE INIT 后输出 READY。协议、SPNEGO/NTLMv2、签名和加密属于固定上游依赖；
本程序拥有 FUSE 节点、远程句柄、errno 与断线代际，结果不确定的写操作不自动重放。
不提供 SMB1、Kerberos、POSIX 扩展或 NetBIOS 发现；设计与验证边界见
[网络驱动器](../../../docs/architecture/network-drives.md)。
