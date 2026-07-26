# Audio runtime fixtures

这些固定输入只用于 host decoder tests 与 AArch64 QEMU production Music Player gate。除 MP1
conformance vector 外，全部文件由 FFmpeg 7.1 从同一个 44.1 kHz stereo、2 秒双正弦信号生成：
左声道 440 Hz，右声道 660 Hz，幅度 0.16。门禁校验解码后为 48 kHz stereo finite PCM 且非静音，
并由 QEMU WAV backend 验证实际 device 输出。

生成源：

```text
aevalsrc=0.16*sin(2*PI*440*t)|0.16*sin(2*PI*660*t):s=44100:d=2
FFmpeg 7.1 (imageio-ffmpeg 0.6.0 macOS arm64 wheel)
```

编码/容器矩阵为 PCM WAV、PCM AIFF、PCM CAF、FLAC、MPEG Layer II、MPEG Layer III、Ogg
Vorbis、AAC/M4A、AAC/MP4、ALAC/M4A、Vorbis/Matroska 与 Vorbis/WebM。MP1 fixture 是 ISO
MPEG Layer I `fl1.mpg` conformance vector 的上游 FFmpeg sample archive 副本：

```text
https://samples.ffmpeg.org/A-codecs/mp1-sample.mp1
archive timestamp: 2015-09-05T19:05:43Z
upstream description: "The file fl1.mpg from ISO's MPEG layer 1 audio test vectors."
```

`limiter-dc.wav` 不属于 13 格式矩阵，只用于自动 WAV backend 的 8-stream limiter 压力窗口。
它是 48 kHz stereo S16、10 秒、双声道固定 `8192/32768=0.25` 的 deterministic PCM。固定正电平
让错开的 8 个 production process 也必然越过 limiter threshold；该文件不进入 CoreAudio 人工播放。

SHA-256：

```text
06d9f7cd8b6348f6800132d56daa09eda6c8ff38450be7a56f7a3284da48e424  limiter-dc.wav
31c4df4b38372d2941e196282b221a0d8d41c14f42bf489e4fc39688adb07739  tone-aac.m4a
b78003d7924d79124d932c4a5c7440368b3e218e2e61be68dc7c08c4fb83d912  tone-alac.m4a
5b4d3dae424cb7420a35a136fb6b5ee351d82dc1d8f29ab1190f8f6e4b559993  tone.aiff
2866a1c29d8af3e0ba3aaa72a714df40fa2ef8098ac1111eb07379fa3ca1373c  tone.caf
8a0c125b578e18ff12e44a4a3eb2c771bff74736395d53efd0a10f57064cfe38  tone.flac
6b2dcde13da82f434f2428de7e32ac7bb1a9c7b997ffeec3936a6db79abe3d90  tone.mka
8bffe46c1a1ba709b35f4782c1a661d0081e8ef84222edcf518ee5e4c2e77f13  tone.mp1
4374fe30f22780aa584d618e8721f3de84affdf2fadf461e3355ff1248ff92f5  tone.mp2
884387cec127c35e3fc8d035817186e1fcdc4d18a5b33073ef4bbbb6d3956e53  tone.mp3
429eda8740599eb6d59c2c682f892d0088b6b7231370b1875c0cabfe95732f1b  tone.mp4
9ae2c399cf7c7d67deb8eaceb2c665f8e88f2854315baa2f11d193ef912d1d95  tone.ogg
c3f244f5304e1161d2c9c33b36092faf6b9696dce4f76bd4113f24aed810b2ee  tone.wav
72139498f2a13122c6716ee006a4c6f143f15eb457cf815d83f0839ba878c287  tone.webm
```
