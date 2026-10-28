# 映射网络驱动器（SMB，实施中）

Status: Active

目标：资源管理器按 Windows 2000“映射网络驱动器”把 SMB 共享挂进文件系统，所有程序经普通路径访问。

已确认的方向（2026-10-28）：

- 内核实现标准 Linux FUSE UAPI；SMB 客户端在用户态，不做内核 CIFS，不设私有挂载接口。
- 普通用户经上游 setuid `fusermount3`（固定 Alpine fuse3 APK）挂载，不另建 root 代挂服务。
- 协议 SMB 2.1、3.0、3.0.2、3.1.1，NTLMv2 认证、签名、3.1.1 预认证完整性与 AES 加密；
  不做 SMB1、Kerberos/域登录。
- 运行时验收用固定版本上游 Samba，在宿主以非 root 启动临时共享，分别强制签名与强制加密。

## 依据

- FUSE：本地固定 Linux 7.1 的 `include/uapi/linux/fuse.h`（协议 7.45）、`fs/fuse/`、
  `Documentation/filesystems/fuse.rst` 与 libfuse `fusermount3` 的挂载约定。
- SMB：MS-SMB2、MS-NLMP、MS-SPNG；版本与摘要在实施时固定到 standards-baseline。
- 界面：Windows 2000 简中 ISO 的 shell32/mpr 对话框与字符串资源，由唯一导入器提取。

## 阶段与验证点

1. 内核 FUSE（进行中）
   - 已完成（2026-10-28，`make build-kernel`、kernel/kernel-unit clippy、编解码与选项单元测试、architecture-check 通过；
     guest 探针 `scripts/fixtures/musl/fuse.c` 在 BusyBox 门禁 1/2 CPU 启动中通过）：`/dev/fuse` 与连接、INIT、请求/回复/中断/中止、FORGET/BATCH_FORGET 预留、
     LOOKUP/GETATTR/SETATTR/OPEN/READ/WRITE/RELEASE/OPENDIR/READDIR/READDIRPLUS/MKNOD/MKDIR/SYMLINK/LINK/UNLINK/
     RMDIR/RENAME/RENAME2/READLINK/STATFS/FSYNC/FSYNCDIR/FALLOCATE/ACCESS、entry/attr 超时缓存、通知
     INVAL_INODE/INVAL_ENTRY/DELETE、FUSE_DEV_IOC_CLONE、MAX_PAGES、PARALLEL_DIROPS、READDIRPLUS(_AUTO)。
     VFS 补齐：per-open `StorageOpen`、`CallerFiles`/`Requester`、子类型、`SuperblockFacts`（mount table 不再 statfs）、
     `MAY_ACCESS`/`MAY_CHDIR`、`FileSystemError::Errno`、`page_cache::invalidate`、无 I/O 的 `Inode::identity`、
     victim 通知 `Inode::unlinked`，以及缺失的 `truncate(2)`。
   - 已完成：页缓存多页预读与 ASYNC_READ、AUTO_INVAL_DATA（`Inode::revalidate_data`、非阻塞作废）。
   - 已完成：`close(2)` 的 FLUSH（`StorageOpen::flush` 与唯一 `filp_close` 路径）；ext4 延迟 orphan 回收接入 sync
     （Codex 协助定位，BusyBox 门禁含掉电 e2fsck 全部通过）。
   - 已完成：O_CREAT 的 CREATE 原子打开（`Inode::create_open`）。
   - 已完成：WRITEBACK_CACHE（页缓存缓冲写、close/最后句柄写回；修复 mmap 脏页在句柄释放后丢失）。
   - 已实现、运行验证待执行：目录级 VFS 锁与短 topology/publication 事务；FUSE 等待不持全局锁，
     O_CREAT 查找/创建同次持父目录独占锁，remount 后端准备在 topology gate 外。
   - 已实现、运行验证待执行：GETXATTR/SETXATTR/LISTXATTR/REMOVEXATTR，长度查询、空值、CREATE/REPLACE、
     逐操作 ENOSYS 缓存、列表格式校验；沿用 VFS user/trusted/security 与 capability policy，普通 DAC 经
     实际 filesystem 判定。guest 探针增加非特权 caller、错误与缓存边界；按用户要求不执行回归或 QEMU。
   - 已实现、运行验证待执行：FOPEN_DIRECT_IO 按 per-open fh 绕过内容页缓存，覆盖 read/write/vector、
     positioned I/O、sendfile/copy 与 io_uring worker；短读不修改 EOF，分块写错误保留已提交前缀。
     混合缓存写先撤销共享/未 COW 私有映射、写回并作废相关页；未协商 DIRECT_IO_ALLOW_MMAP，
     MAP_SHARED 返回 ENODEV，MAP_PRIVATE 保留已有 COW。guest 探针已扩展，按用户要求不运行。
   - 已实现、运行验证待执行：标准 fusectl 挂载到 /sys/fs/fuse/connections，提供实际连接目录与
     waiting/abort/max_background/congestion_threshold；请求令牌、真实后台预算、INIT 协商与拥塞预读相接。
     已更新 guest 探针，覆盖预算暂停/放行、在途 abort/新请求 errno、卸载后旧 fd 与两种 ABORT_ERROR 协商。
   - 阶段 1 剩余：FUSE over io_uring、passthrough；探针随各项扩展。
   - `/dev/fuse`（10:229）：每次 open 一个连接；read 取请求、write 回复、poll；守护进程关闭 fd 即中止连接，
     未完成请求返回 ENOTCONN。
   - `mount -t fuse[.subtype]`：`fd=`、`rootmode=`、`user_id=`、`group_id=`、`default_permissions`、
     `allow_other`、`max_read=`；`/proc/mounts` 显示 `fuse.<subtype>` 与 source。类型查找需支持 `fuse.` 前缀。
   - 操作：INIT、LOOKUP、FORGET/BATCH_FORGET、GETATTR、SETATTR、OPENDIR/READDIR/RELEASEDIR、
     OPEN/READ/WRITE/FLUSH/FSYNC/RELEASE、CREATE、MKDIR、UNLINK、RMDIR、RENAME2、STATFS、INTERRUPT、DESTROY。
   - 数据走全局页缓存（mmap 与执行可用）：打开时按 FOPEN_KEEP_CACHE 决定是否作废缓存；
     写入语义（write-through 或协商 FUSE_WRITEBACK_CACHE）需先核对页缓存写路径后确定。
   - 阻塞请求可被信号中断并发送 FUSE_INTERRUPT；未声明 allow_other 时只有挂载用户可访问。
   - 性能特性（INIT 协商）：FUSE_MAX_PAGES（单请求 1 MiB）、FUSE_ASYNC_READ 预读、FUSE_WRITEBACK_CACHE、
     FUSE_DO_READDIRPLUS/READDIRPLUS_AUTO、entry/attr 超时缓存、FUSE_AUTO_INVAL_DATA、FUSE_PARALLEL_DIROPS、
     多线程守护进程（FUSE_DEV_IOC_CLONE）。
   - `/sys/fs/fuse/connections/<dev>/`：`abort`、`waiting`、`max_background`、`congestion_threshold`，
     用于中止卡死的网络挂载与调整后台并发。
   - FUSE over io_uring（FUSE_IO_URING，IORING_OP_URING_CMD 注册/提交）：复用现有 io_uring 子系统，
     守护进程以 ring 代替 /dev/fuse read/write 往返。
   - passthrough（FUSE_PASSTHROUGH，FUSE_DEV_IOC_BACKING_OPEN/CLOSE）：读写与 mmap 直达登记的本地后备文件；
     对 SMB 无收益，为套在本地文件上的 FUSE 文件系统提供。
   - 验证：kernel-unit 覆盖请求/回复编解码、连接中止与中断；guest 最小 FUSE 探针分别走 /dev/fuse、
     io_uring 与 passthrough 三条路径。
2. fusermount3：安装 setuid 二进制与 `/etc/fuse.conf`；核对其实际使用的 syscall。
   验证：普通用户挂载、访问、`fusermount3 -u` 卸载探针。
3. SMB2/3 客户端库：NEGOTIATE 上下文、SPNEGO/NTLMSSP v2、签名（HMAC-SHA256 / AES-CMAC）、
   SP800-108 密钥派生、AES-128-CCM/GCM 传输加密、credit、CREATE/CLOSE/READ/WRITE/QUERY_DIRECTORY/
   QUERY_INFO/SET_INFO/FLUSH/ECHO。加密原语使用固定版本 RustCrypto crate。
   验证：MS-NLMP 与 MS-SMB2 附带测试向量的单元测试。
4. SMB FUSE 守护进程：FUSE 请求到 SMB 调用的翻译、断线重连、错误码映射。
5. 资源管理器：“工具 > 映射网络驱动器/断开网络驱动器”、凭据对话框（含“保存密码”）、
   “我的电脑”网络驱动器项与图标、登录时重新连接。挂载点位置在实施时核对会话环境后确定。
   - 凭据按 `mount.cifs` 的 `credentials=` 文件格式（`username=`/`password=`/`domain=`）每个映射一份，
     存于 `~/.config/liteos/network-drives/`：目录 0700、文件 0600，原子写入；断开映射或取消保存时删除。
     守护进程从该文件或 stdin 读取，密码不出现在命令行、环境变量或 `/proc/<pid>/cmdline`。
   - 风险：与 Linux 的 smbcredentials 相同，只靠文件权限保护，root 与能读取用户主目录的备份可见明文；
     LiteOS 没有 Secret Service/DPAPI 一类系统密钥环。
6. 运行时门禁：固定 Samba 构建在 `.local/`；签名与加密两种配置下的挂载、读写、重命名、删除、卸载。

## 范围限制（实施时同步到 ABI 矩阵）

- 不支持 fuseblk（块设备上的 FUSE，如 ntfs-3g）与 CUSE（用户态字符设备）：与性能和网络驱动器无关，
  需要时另立计划。
- 不支持 virtio-fs 及其 DAX 窗口（FUSE_SETUPMAPPING/REMOVEMAPPING）：QEMU vhost-user-fs 依赖宿主 virtiofsd，
  上游只支持 Linux 宿主，当前 macOS/HVF 开发机无法运行验收；与网络驱动器无关。
- 服务器名只经 DNS/hosts 解析，不做 NetBIOS 广播名称解析。
