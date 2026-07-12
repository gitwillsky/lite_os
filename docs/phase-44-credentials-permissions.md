# Phase 44：身份与文件权限竖切

Phase 44 以 Linux v7.1、POSIX.1-2024、musl 1.2.6 与 BusyBox 1.37.0 为固定语义源，删除固定 root policy。范围限于当前单 user namespace、单 mount namespace；Linux capabilities、ACL、LSM 与 idmapped mounts 不在本阶段伪造。

## 唯一模型

- `Process::credentials` 是 real/effective/saved UID/GID、supplementary groups 与 umask 的唯一 owner；Thread 共享，fork 复制，exec 只在新映像不可失败提交点应用最终 ELF 的 set-id bits。
- VFS 的 `AccessIdentity` 是无状态快照，不复制 credential state。VFS 唯一执行 pathname search、inode rwx、parent write+search、sticky directory、protected hardlink 与 setgid-directory inheritance policy。
- ext2 只持久化 VFS 已决定的 mode/uid/gid/ctime；低/高 16-bit UID/GID 与 mutation 都进入现有 JBD2 transaction，不存在非 journal metadata 旁路。
- syscall 只解码 riscv64 UAPI、copyin/copyout 与 errno；signal permission 位于 process graph 的目标选择 seam。

## ABI 与可观察语义

新增标准入口为 `fchmodat/fchownat`、`setgid/setuid`、`setresuid/getresuid`、`setresgid/getresgid`、`getgroups/setgroups` 与 `umask`。既有 open/create/chdir/access/exec/link/unlink/rename 与 kill/tkill/tgkill 全部消费同一 credentials/VFS policy。root execute 仍要求至少一个 execute bit；signal zero probe 也执行 permission check；SIGCONT 保留 same-session 例外。

BusyBox rootfs 唯一启用 `id/chmod/chown`，动态 musl probe 覆盖 umask、chmod/chown、supplementary groups、非 root open denial、signal denial与 setuid dynamic exec。完整验收仍只有 `make verify`。
