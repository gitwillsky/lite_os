# media-player

- **类型**：程序（可执行文件）
- **作用**：媒体播放机：播放 16 位 PCM WAV 文件
- **产物 / 安装位置**：`/bin/mplay [文件]`
- **对应的 Windows 2000 组件**：sndrec32.exe（录音机）的播放部分
- **依赖**：classic-ui、classic-theme、canvas、typeface、sound、liteos-runtime

显示整段波形、位置与长度，可播放、停止、回到开头，拖动滑块定位；"文件→打开"用单行输入框键入路径。
资源管理器按扩展名 `wav` 经桌面条目 `MimeType=audio/x-wav` 把文件交给它。播放在后台线程里写 ALSA 子流，
设备缺失或被占用时在状态栏显示原因。没有录音（内核只有播放子流）与 MP3/MIDI 等其他格式。
