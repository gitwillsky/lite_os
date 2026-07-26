# ADR: AF_UNIX 控制面与 memfd SPSC PCM ring 分离

- 状态：已接受
- 日期：2026-07-26

## 背景

48 kHz stereo float PCM 每条 stream 约 384 KiB/s。持续把 PCM 写入 AF_UNIX 会产生额外 user-copy、
kernel buffer copy、syscall 和 wakeup；让客户端直接共享 mixer 内部状态又会失去 owner 与 cleanup 边界。

## 决策

- AF_UNIX stream 只承载版本握手、stream create/start/pause/flush/close、gain、消费进度和 typed error。
- 系统音频服务为每条逻辑 stream 创建标准 Linux `memfd`，经 `SCM_RIGHTS` 传给 LiteUI audio worker。
- 双方以 `mmap(MAP_SHARED)` 映射固定容量 SPSC PCM ring；worker 是唯一 producer，mixer 是唯一 consumer。
- ring 只保存 [PCM 正规形](audio-pcm-normal-form.md)，不携带 container、codec 或任意格式 tag。
- producer 只在 empty→nonempty，consumer 只在 full→available 边缘经控制 socket 通知；双方不得 spin/poll。
- `memfd_create`、seal/size、mmap 与 close 使用固定 Linux UAPI；不增加 private kernel shared-memory object。
- seek/`load()`/source replacement 使用递增 generation：worker 先请求 mixer 停止并 flush，mixer 确认
  不再读取旧 PCM 后双方原子切换 generation，再复用同一 ring。late completion 只能匹配旧 generation，
  不得推进新 source/time。

## 结果

- steady PCM 数据不经过 socket payload copy，也不分配。
- ring index、wrap、publish/acquire order、disconnect、flush generation 和 fd cleanup 必须有 model/unit test。
- client 不可写 header/consumer index 之外的 service-owned 状态；畸形 index 或 generation 永久关闭该 stream。
- stream close、app exit、service restart 和 partial `SCM_RIGHTS` publication 必须 exactly-once unmap/close。
