# 症状路由

本文件只负责把第一个可观察症状路由到区分性探针。模块、接口和 ABI 事实始终以
`docs/README.md` 指向的当前文档为准。

## Host、镜像与构建

| 第一个症状 | 先区分 | 下一 owner |
|---|---|---|
| `development image is already in use` / write lock | 设置 `LITEOS_ARCH` 后执行 `lsof -- "fs-${LITEOS_ARCH}.img"`；确认 QEMU 与同步进程；private copy 是否复现 | `scripts/sync_userland.py`、Makefile |
| `ENOSPC` | guest RAM、host 文件长度、ext superblock 容量、guest `df`、安装峰值分别取证 | `scripts/ext2_image.py`、`scripts/resize_ext2_image.py`、`scripts/verify_busybox.py`、`scripts/apk_rootfs.py` |
| 构建成功但 guest 仍是旧内容 | baseline 与开发实例、cache identity、`sync-userland` destination 分别核对 | Makefile、`scripts/sync_userland.py` |
| QEMU 启动前退出 | 完整 QEMU command、artifact architecture、image lock、host signal | `scripts/build_target.py`、`scripts/qemu_gate.py` |

先读取当前配置，不记忆默认值：

```bash
rg -n '^(ARCH|ACCEL|PROFILE|FS_IMAGE_SIZE_MIB|QEMU_MEMORY|QEMU_SMP) \\?=' Makefile
LITEOS_ARCH=aarch64
file "target/rootfs/${LITEOS_ARCH}.img" "fs-${LITEOS_ARCH}.img"
LITEOS_IMAGE="fs-${LITEOS_ARCH}.img" PYTHONPATH=scripts python3 -c \
  'import os; from pathlib import Path; from ext2_image import ext2_capacity_bytes; print(ext2_capacity_bytes(Path(os.environ["LITEOS_IMAGE"])))'
```

容量判断必须同时报告 host 文件长度、ext filesystem capacity、guest 可用块和 workload 峰值；
其中任意一个不能代替其他三个。

## Guest 入口与进程

| 第一个症状 | 先区分 | 下一 owner |
|---|---|---|
| `command not found` | 文件 inode、mode、PATH、shebang interpreter、ELF interpreter、target architecture | rootfs、`user/base/`、userspace ABI |
| `illegal instruction` | shell 名称是否正确、ELF machine、用户态 PC/instruction、trap decode、CPU feature | userspace ABI、`kernel/src/trap/`、`kernel/src/arch/` |
| spawn/fork 返回 `EAGAIN` | 精确 syscall 返回点、资源限制、single/multithread、child 是否已创建 | process syscall、task graph、process ABI |
| launcher 失败但 native ELF 成功 | shebang、runtime、PATH、spawn/exec；native 主体退出路径暂时移出范围 | user session、process ABI |
| native ELF 也失败 | 最小 musl fixture、ELF loader、syscall trace、相同输入 | userspace ABI、对应 syscall/domain |
| reboot 后行为改变 | 写入落在哪个 image、init/profile、cache stamp、journal recovery | rootfs、init、filesystem |

入口差分固定沿同一安装产物进行：

```text
package manager → installed launcher → language runtime → bundled native ELF
→ 最小 process fixture → 具体 syscall
```

任何一步转绿，都把该步下方已经真实执行的层级移出当前红色边界；不得用另一次安装或另一版本充当
绿色对照。

## Kernel 活性与并发

| 第一个症状 | 先区分 | 下一 owner |
|---|---|---|
| QEMU 运行中退出 | 串口 panic、guest poweroff、host assertion/signal、最后一个动态 marker | trap、arch/platform、触发该事件的 domain |
| 无输出 hang | QEMU 是否存活、CPU 是否前进、task wait、deferred bit、IRQ/lock owner | execution、process scheduling、device domain |
| SMP 才失败 | per-CPU state、remote ack、IPI、TLB、CPU migration、lock ordering | task、memory、arch/platform |
| timeout 后后续命令失败 | timeout cleanup、wait membership、child/FD/socket owner 是否释放 | 原 syscall domain、task graph、IPC/network |
| epoll/poll 路径 panic 或丢 wake | source notifier 的上下文、level recheck、wait registration、backend owner | filesystem readiness、IPC/network、task sync |
| page fault/COW/stack churn 后失败 | PTE mutation、translation fence、frame retirement、active CPU set | memory、arch MMU、platform IPI |

并发问题至少做一组控制变量：

```text
SMP1 ↔ SMP2+
task context ↔ deferred/notifier context
owner uncontended ↔ owner contended
local CPU ↔ migration/remote CPU
publication ↔ retirement/cleanup
```

对照命中后，进入 `kernel-owner-map.md`，按实际调用链选择 owner；症状名称本身不决定修复模块。
