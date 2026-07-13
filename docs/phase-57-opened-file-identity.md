# Phase 57：打开文件身份与标准 fd namespace

本阶段按固定 Linux v7.1 `fs/namei.c`、`fs/proc/fd.c` 与 POSIX rename/unlink 语义，把 pathname lookup 的结果从裸 inode 提升为 VFS-owned opened entry。目标是让 cwd、directory fd、regular/character OFD、`/proc/<pid>/fd` 和 `/dev/fd` 共享同一目录项身份，不通过写死 console 路径或缓存绝对字符串伪造结果。

## 唯一状态 owner

- `OpenedFile` 持有 inode 与 parent/name/deleted 目录项关系；VFS weak registry 是 rename/unlink 唯一更新入口。hardlink 的不同目录项分别建立 opened identity，dup/fork 则继续共享原 OFD 与同一 identity。
- 路径从 opened-entry parent 链实时投影。ancestor rename 会更新共享链节点；任一链节点 unlink 后，procfs target 追加标准 ` (deleted)`，而 `getcwd` 返回 `ENOENT`。
- Pipe 与 Socket 分别持有一次分配且与 `fstat.st_ino` 一致的 runtime object identity；fd table 只投影，不按 descriptor 复制 identity。Epoll 使用 Linux 固定 `anon_inode:[eventpoll]` label。
- procfs fd 节点是 kernel magic link：pathname-backed fd 直接跟随 live opened entry，因此 `/dev/fd/N` 在原目录项删除后仍引用已打开文件；`readlinkat` 仍返回用户可见的 pathname/anonymous label。

## Namespace 与 userspace

- `/proc/<pid>/fd` 动态列出 live descriptor，并公布 pathname、`pipe:[id]`、`socket:[id]` 或 `anon_inode:[eventpoll]` target。
- fd directory 访问使用当前 credential model 的 ptrace-read 边界：同 TGID、effective root 或 caller effective UID 与目标 real/effective/saved UID 全部相同；不匹配返回 `EACCES`。
- 唯一 devfs 发布 `fd -> /proc/self/fd` 以及 `stdin/stdout/stderr -> /proc/self/fd/{0,1,2}`；这些节点不写入会被 devfs mount 遮蔽的 ext2 `/dev`。
- 固定 BusyBox 1.37.0 开放 `tty` applet。musl `ttyname()` 通过真实 `/proc/self/fd/0` 与 devfs metadata 得到 `/dev/console`，kernel 不提供 applet 专用旁路。

## Guest gate

独立 disposable image 由 BusyBox init 直接运行 fail-fast Phase 57 script，验证：

1. `tty`、动态 fd directory、pipe label、`/proc/self/fd/0` 和 `/dev/{fd,stdin}` 的标准目标；
2. fd target 随 rename 更新，hardlink 两个 opened identity 保持各自名称；
3. unlink 只把被删除目录项标记为 ` (deleted)`，同 inode 的其他 hardlink 保持可达；
4. `/dev/fd/N` 写入、已删除文件的 live fd I/O、close 后 fd 节点消失，以及 `/dev/stdout`。

host 只构造隔离镜像、启动 QEMU、拒绝 kernel error 并等待最终 `LITEOS_OPENED_FD_57`，不解析中间 shell 输出替代客体语义判断。
