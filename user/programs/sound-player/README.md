# sound-player

- **类型**：程序（可执行文件）
- **作用**：播放一个系统提示音后退出
- **产物 / 安装位置**：`/bin/sndplay <文件名>`
- **对应的 Windows 2000 组件**：sndrec32/PlaySound 的独立播放进程（Win2000 的 PlaySound 在进程内，这里用独立进程隔离故障）
- **依赖**：sound、liteos-runtime

桌面、资源管理器通过 `sound::play` 启动它。解码或音频设备出错（含 panic，用户态是 `panic=abort`）只结束这个短命进程，
不会让调用者退出。只接受 `/usr/share/liteos/sounds/` 下的文件名。
