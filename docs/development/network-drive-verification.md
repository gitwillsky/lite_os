# 网络驱动器：构建与验证

## 产物与来源

| 产物 | Owner | 安装位置 |
|---|---|---|
| SMB2/3 FUSE 守护进程 | `user/programs/smbfs` | `/bin/smbfs` |
| 映射、凭据与连接生命周期 | `user/libraries/network-drive` | 程序内部依赖 |
| 固定 Alpine fuse3 helper | `scripts/fuse_runtime.py` | `/bin/fusermount3`，root:root 4755 |
| 私有挂载策略 | `user/base/fuse.conf` | `/etc/fuse.conf` |
| 原版标签与网络驱动器图标 | `scripts/extract_windows2000_assets.py` | `assets/classic/`，由 manifest 管理 |

固定版本、摘要与协议来源见 [规范基线](../standards-baseline.md)；所有下载、源码与验收身份放在 `.local/`。
`make build` 通过现有 userspace 编译路径与唯一 `base_userland` 清单发布程序/helper，开发镜像同步消费同一清单。
安装器用 regular-file 类型与完整 Unix 权限位组合；新建 rootfs 与开发镜像同步共用镜像 inode 校验，检查类型、root:root 与含 setuid 的完整权限。
文件内容单独 dump 后比较摘要，不以宿主导出文件的权限裁决镜像权限；debugfs 的 dump -p 会丢失特殊权限位，不能以普通 0755 代替 helper。

## 产品入口

资源管理器“工具 > 映射网络驱动器”按 netplwiz 对话框 106 的向导页布局输入盘符与 UNC、选择登录重连；“使用其他用户名进行连接”
打开对话框 124 的“连接身份...”输入 `域\\用户`、密码，并在其中选择 LiteOS 扩展项保存密码与要求 SMB 3 加密。“浏览”因没有网络邻居枚举而禁用，
“断开网络驱动器”按 mprui 对话框 7002 用多选列表断开所选盘符。
“我的电脑”双击网络驱动器经同一生命周期恢复连接；“工具 > 断开网络驱动器”卸载并删除记录/密码。
凭据未保存的离线映射需断开后重新输入；不会静默尝试空密码。

命令行入口也只委托同一 `network-drive` owner；凭据来自受权限保护的文件或 stdin，不放进参数或环境变量：

```bash
smbfs map Z //server/share sign save reconnect < /path/to/private-credentials
smbfs disconnect Z
smbfs reconnect
```

`sign` 允许服务器要求加密；`encrypt` 强制使用 SMB3 加密。`save|session` 和 `reconnect|once` 是显式选项，
`session reconnect` 会被拒绝。底层 `serve SOURCE MOUNTPOINT sign|encrypt` 只在认证并处理 FUSE INIT 后输出 READY。

## 运行时门禁

运行时门禁、回归与 benchmark 的结果尚待实际验证。
Samba 构建需要宿主 C 编译器、make、Python、pkg-config 与 GnuTLS 开发环境；不使用系统滚动版本的 smbd。

构建与运行入口集中在[构建与验证](build-and-verify.md#运行时门禁)。

- `scripts/samba_gate.py` 固定 Samba 4.22.11，普通宿主用户启动高端口临时共享；拒绝 root 启动。
  每次创建当前 Unix 用户的私有 passdb、随机密码、状态与日志目录；密码仅经 stdin 和 0600 文件传递。
  自己创建的进程组在退出时精确停止，不修改系统 Samba 配置或账户数据库。
- 门禁依次强制签名、强制加密，使用独立镜像和 UID 65534 经过产品 `smbfs map`、上游 setuid helper、
  真实 FUSE/SMB 读写、跨包 EOF、共享 mmap/msync、truncate、rename、打开后 unlink、网络路径 exec，
  然后验证保存凭据重连、会话凭据不落盘、卸载与删除凭据。源磁盘不改写。
- 成功必须同时出现 `LITEOS_SMB_FILES_42` 与 `LITEOS_SMB_42`；日志写到
  `target/network-drive-runtime/<arch>/`，失败不能发布成功结论。冷缓存先构建固定 Samba，重跑不能换成系统副本。
- `scripts/fixtures/musl/fuse.c` 随现有 BusyBox 门禁运行，新增 passthrough 阶段保护登记权限、独立 OFD offset、
  后备读写、注销后旧句柄/mmap 存活与新 OPEN 失败；其 io_uring 与传统 FUSE 阶段保留在同一探针内。
- 手工断开/重启服务器时核对：本次失败的修改没有被重放、旧 fh 返回 ESTALE、新打开可重新认证；
  不把已缓存的目录/内容作为连接健康证明。完整成功仍需覆盖两个目标架构与所支持的协议版本。
