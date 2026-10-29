# 网络驱动器当前架构

## 当前设计

- 内核只提供标准 Linux FUSE，SMB 协议在普通用户的 `/bin/smbfs` 中执行；所有应用经普通文件路径、
  页缓存与 mmap 访问共享，不需要接入网络文件私有 API。内核边界见 [FUSE 契约](../architecture-contract/fuse.md)。
- `network-drive` 独占每用户映射记录、凭据与挂载生命周期。盘符范围 D–Z，实际路径为 `~/Network/<盘符>`；
  记录位于 `${XDG_CONFIG_HOME:-$HOME/.config}/liteos/network-drives/`，实际挂载状态来自 `/proc/mounts`。
  标准挂载源标签为 `SMB`；UNC 不拼入 helper 选项，含逗号的共享名不会变成挂载选项。
  映射文件不冒充在线状态；失联的旧 FUSE 挂载经标准 helper 卸载后重建。
- 资源管理器“工具”菜单提供映射/断开；“我的电脑”展示在线或断开的网络驱动器。双击会恢复保存过凭据的连接。
  桌面启动在后台恢复显式设置“登录时重新连接”的映射，未勾选的记录只在访问时恢复。
  “我的电脑”每秒从本地记录和 mount table 更新变化的映射盘，保留行身份、排序和地址输入，不主动探测共享。
  网络认证与文件读取不阻塞 UI 线程。
  控件使用共享 UI SDK；标签与图标由唯一 ISO 导入器生成并纳入 `assets/classic/manifest.json`。
- 上游 `fusermount3` 是唯一普通用户挂载入口，安装权限为 root:root 4755；`/dev/fuse` 为 0666，
  `/etc/fuse.conf` 不启用 `user_allow_other`。init 将 `/etc/mtab` 指向内核 `/proc/mounts`，不维护另一份挂载表。
- SMB 客户端固定为 smb-rs 0.12.1，协议覆盖 SMB 2.1、3.0、3.0.2、3.1.1。SPNEGO/NTLMv2、
  credit、HMAC-SHA256/AES-CMAC/AES-GMAC 签名、预认证完整性、SP800-108 密钥派生与 AES-128-CCM/GCM
  由固定上游及 RustCrypto/SSPI 依赖实现。禁用匿名无签名访问；要求加密时连接不支持的服务器直接失败。
  唯一依赖源与修改记录见 [规范基线](../standards-baseline.md)。
- `smbfs::State` 独占会话、路径节点、lookup 引用与远程文件句柄。FUSE 目录 offset 绑定一次 opendir 的快照；
  标准 READDIRPLUS 直接复用 QUERY_DIRECTORY 的属性，只有进入回复的非点目录项增加 lookup，失败整包撤销。
  内核名字/属性缓存最多 1 秒，目录快照不会延长已过期的属性；外部变更在窗口后重新查询。
  SMB 目录记录不含 nlink，批量回复投影为 1，GETATTR 仍读取服务器实际值。
  rename 同步移动节点路径，unlink 保留已打开句柄至 release，FORGET 与 release 共同决定节点回收。
  文件读按 SMB 请求边界合并，不能将单包边界误报为 FUSE 的 EOF；EOF 与长度均由服务器事实决定。
- 传输/完整性失败关闭本代连接并撤销句柄；旧 fh 返回 ESTALE，后续路径操作才重新认证。
  结果不确定的修改不自动重放。NTSTATUS 映射为文件系统 errno，READ/WRITE 回复长度必须落在原请求范围内。
  可写句柄的 FLUSH/fsync 直达服务器，只读关闭不向服务器发送写入 flush；release 与临时资源的析构关闭远程句柄；正常销毁关闭剩余句柄与会话。
- 认证只经 stdin 或私有 credentials 文件传递，密码不进入 argv、环境变量或 cmdline。目录为 0700、文件为 0600，
  同目录独占临时文件写入、sync、rename；每用户 OFD flock 串行化多窗口连接/断开。断开删除记录与密码，
  卸载失败保留记录供重试；登录重连必须保存密码。应用拥有的密码缓冲在释放前擦除。
- Benchmark 决策：采用 64 KiB SMB 写入/预读上限与单会话请求 owner；复用内核页缓存，不另建内容缓存。
  网络请求与目录快照会分配、加锁；吞吐、延迟、断线恢复成本留待真实 Samba 验收测量，不声明性能收益。

## Known limits

- 凭据文件使用 mount.cifs 的 `username`/`password`/`domain` 格式，以文件权限保护；root 和用户备份可见明文，
  没有系统密钥环。会话保持认证信息以供断线重连，不记录或打印认证输入。
- 不支持 SMB1、Kerberos、域登录、DFS、多通道、压缩或 NetBIOS 广播发现；服务器名经 DNS/hosts 解析。
  `domain` 只是 NTLM 用户名限定，不代表本机加入域；可指定 `server:port`，未指定端口时使用 445。
- 共享权限以 Windows ACL 为准，合成当前用户的文件模式；不伪造 chmod/chown 成功，不提供 POSIX 扩展、
  服务端硬链接、symlink、分布式字节范围锁或远程 xattr。目录快照不订阅远端变更通知，重新打开目录读取新列表。
- 本次实现只做编译、严格 Clippy、Rust AST/架构检查和静态 Review；运行、回归与 benchmark 按用户要求未执行。
  普通用户挂载、各协议及签名/加密互操作、SMP 与断线行为仍须按
  [网络驱动器验证](../development/network-drive-verification.md) 实际验收，不能把静态通过当作运行成功。
