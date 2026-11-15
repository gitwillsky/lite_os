# media-player

- **类型**：程序（可执行文件）
- **作用**：媒体播放机：Windows 2000 外观、IINA 级能力的音视频播放器
- **产物 / 安装位置**：`/bin/mplay [文件或网址…]`
- **对应的 Windows 2000 组件**：mplayer2.exe（Windows Media Player 6.4）
- **依赖**：liteos-ui、mpv-client

解码、同步、字幕渲染与音频输出由上游 libmpv/FFmpeg 完成（`scripts/mpv_runtime.py` 安装，AArch64）；播放器只用共享控件：
菜单栏、视频画布（Wayland 视频子表面）、进度/音量滑块、按钮、播放列表（可隐藏侧栏）与状态栏。
功能：文件/网址/外挂字幕、播放列表（上一个/下一个、双击跳转）、轨道选择（视频/音频/字幕）、章节、0.25–4 倍速、
A-B 循环、单个文件/列表循环、逐帧、画面比例与旋转、音量/静音/音频延迟、字幕延迟与缩放、截图（保存到“图片”）、
全屏（F11/双击）、媒体信息、鼠标点击暂停与滚轮调音量。常用键：空格、←/→（Shift 为 1 分钟）、↑/↓、M、F/F11、,/.、N/P、[ ]。
libmpv 软件渲染直接写入 64 字节对齐的 XRGB 槽。播放器依据 Wayland linux-dmabuf v4 feedback 打开兼容的 VirtIO-GPU，
在渲染线程异步上传，合成器导入同一缓冲并通过 viewport 裁除行尾填充。使用发布后的槽需同时等 GPU fence 与 wl_buffer.release。
没有兼容设备时，沿用同一子表面提交封印的 SHM 槽。缺少 libmpv（如 riscv64）时启动即弹出原因并退出。
