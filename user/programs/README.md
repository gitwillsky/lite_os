# user/programs — 可执行程序

这里每个目录都是一个会被安装到 rootfs 的 **程序**（有 `src/main.rs`）。划分参照 Windows 2000：
外壳只含桌面、任务栏、开始菜单与托盘，任务管理器、音量控制、控制面板、资源管理器都是独立程序，外壳只负责启动它们。

程序在动态构建时只链接 `user/libraries/` 里的共享库（NEEDED 为 `libliteos_runtime.so` 及用到的库），不内嵌基础库。

| 目录 | 作用 | 安装位置 |
|---|---|---|
| [`compositor/`](compositor/README.md) | 显示服务与窗口管理器 | `/bin/compositor` |
| [`logon/`](logon/README.md) | 本机登录与会话监督 | `/bin/logon` |
| [`desktop/`](desktop/README.md) | 用户桌面外壳 | `/bin/desktop` |
| [`file-explorer/`](file-explorer/README.md) | 资源管理器 | `/bin/file-explorer` |
| [`control-panel/`](control-panel/README.md) | 控制面板小程序 | `/bin/control-panel <system|datetime|winver>` |
| [`task-manager/`](task-manager/README.md) | 任务管理器 | `/bin/taskmgr` |
| [`sound-player/`](sound-player/README.md) | 播放系统提示音（独立进程，隔离故障） | `/bin/sndplay` |
| [`volume-control/`](volume-control/README.md) | 音量控制 | `/bin/sndvol` |

新增程序时：在这里建目录并写 README，加入 `user/Cargo.toml` 的 members 与根 `Cargo.toml` 的 exclude，
在 `scripts/verify_busybox.py` 的安装清单与 `SYSTEM_LIBRARY_PROGRAMS` 登记，并更新 `architecture-check` 的 `PROGRAMS`。
