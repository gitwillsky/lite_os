# ADR: playback stream 使用 per-app 与 session 固定配额

- 状态：已接受
- 日期：2026-07-26

## 背景

每条 playback stream 持有 8192-frame（64 KiB PCM payload + 64-byte header）ring、
decoder/resampler 状态、文件引用和 mixer bookkeeping。
单条资源固定并不能阻止应用创建无界数量的 media element 并耗尽系统。

## 决策

- 每个 app process 最多 8 条 live playback stream。
- 整个 desktop session 最多 32 条 live playback stream。
- `preload="metadata"` 不申请逻辑 stream；真正 `play()` 时才原子申请 app/session 两级配额。
- paused stream 继续占用配额，直到更换 `src`、调用 `load()`、元素卸载或 app exit。
- 超额 `play()` Promise 以 `QuotaExceededError` 拒绝，不发布半初始化 decoder、ring 或 stream identity。
- 系统服务不得抢占、降质或静默关闭既有 stream 为新申请腾位。

## 结果

- 每个 memfd 的 65,600 bytes logical size 占 17 个 4 KiB shared resident pages；32 条 stream
  的 resident mapping backing 固定为 2.125 MiB。当前标准 regular-file shared-mapping seam 还让
  memfd inode 保留同尺寸的 65,600-byte storage，32 条 stream 合计约 2.002 MiB；因此 ring
  storage 的完整上界约 4.127 MiB（不含小型 inode/page-cache metadata），不能把 resident pages
  虚报为全部内存。mixer 每 period 最多处理 32 条 stream。
- app/session quota reservation 必须在 `memfd`/SCM_RIGHTS publication 前完成，任意失败原子回滚。
- disconnect 与 app exit 必须释放两级 membership；generation 防止 late close 命中新 stream。
