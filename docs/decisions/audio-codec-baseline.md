# ADR: 首期启用 Symphonia 0.6.0 全部稳定音频能力

- 状态：已接受
- 日期：2026-07-26

## 背景

音乐播放器必须覆盖无损、常见存量音乐与现代 M4A 等格式。按文件扩展名接受媒体、但在 decode 时才
发现没有 codec，会让 `canPlayType()`、ready state 和错误事件虚报。分别维护多个 decoder framework
又会复制 probe、seek、trim、error 和 fuzz 边界。

## 决策

唯一 decoder framework 固定为纯 Rust Symphonia `0.6.0`。禁用 default feature，并精确启用全部稳定
音频能力：

- container/format：WAV、AIFF、CAF、native FLAC、MPEG audio、Ogg、ISO MP4/M4A、Matroska/WebM；
- codec：PCM、ADPCM、FLAC、MP1、MP2、MP3、Vorbis、AAC-LC、ALAC；
- metadata：ID3v1、ID3v2、APE、RIFF 与 Vorbis Comment；
- AArch64 启用 NEON optimization，RISC-V 使用同一 source 的 portable path。

不启用 experimental subtitle/video feature；Symphonia `0.6.0` 不提供的 Opus 明确不支持。adapter
直接使用 streaming reader/decoder，不引入把整首媒体 decode 到内存的 wrapper。

每类必须同时具备 metadata duration、顺序 decode、bounded seek、EOF、truncated/corrupt input error 和
fixture tests。`canPlayType()` 只根据已实现的 MIME/container/codec combination 返回能力，不根据扩展名
猜测。Opus 和其他未启用格式按标准 media error 路径结束 load。

decoder 必须消费 MP3/Vorbis 等格式记录的 encoder delay 和尾部 padding，使单文件 duration、seek 与
最终 PCM 内容准确。首期不承诺两个独立文件之间 sample-accurate gapless 切换；该能力未来由 Web Audio
或标准 Media Source 调度承担，不能增加私有 playlist engine。

## 结果

- 音乐播放器可以播放已列出的有损、无损和多种 container 本地媒体。
- Symphonia crate graph、版本、checksum、MPL-2.0 许可与一手来源必须进入 standards baseline；
  不能 runtime 下载 codec。
- AAC 许可审查结论（2026-07-26）：Via Licensing Alliance 当前把 AAC-LC decoder 的
  end-user product 制造商或开发者列为需要签约的主体，并明确开源实现本身不替代专利许可；其
  公开费率按售出的 encoder/decoder product 计费，AAC bit-stream 分发本身不收费。因此仓库内
  source、内部 build 与 conformance test 可以保留 AAC-LC，但任何对外销售或分发的 LiteOS
  end-user decoder product 在责任主体取得适用地域的许可前必须 fail-stop，不能把 MPL-2.0
  source license 当作 patent clearance。权威来源为
  [Via LA AAC program 与 FAQ](https://www.via-la.com/licensing-programs/aac/)；适用标准为
  [ISO/IEC 14496-3:2019](https://www.iso.org/standard/76383.html)。这是发布授权边界，不建立
  关闭 AAC 的第二条产品实现。
- decoder 不得调用 system plugin、ffmpeg executable 或 dlopen，也不得在 QuickJS thread 执行。
- 单文件 trim 是 codec correctness；跨曲 gapless 是明确缺口，不得把普通 `ended`→`play()` 虚报为无缝。
