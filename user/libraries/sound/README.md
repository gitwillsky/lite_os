# sound

- **类型**：库
- **作用**：系统提示音
- **产物 / 安装位置**：共享库 `libsound.so`
- **对应的 Windows 2000 组件**：winmm.dll 的 PlaySound
- **依赖**：linux-uapi、liteos-runtime

解码 PCM WAV、重采样到 48 kHz，并后台写入 ALSA 播放子流；设备不可用时静默。
