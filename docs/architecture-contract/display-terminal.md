# Display、input 与 terminal interface contract

本文件定义 kernel device seam 与唯一图形 console userspace Module；当前实现事实见
[display-terminal architecture](../architecture/display-terminal.md)。

## Kernel seam

- DRM 只发布 Linux v7.1 primary-node UAPI；VirtIO-GPU adapter 不穿过 `drm` seam。
- input 只发布 Linux evdev UAPI；VirtIO-input adapter 不穿过 `input` seam。
- PTY、termios、job control、signal 与 poll 语义由各自 kernel owner 唯一维护；userspace 不复制。

## Userspace topology

- `user/` 顶层只允许 `README.md`、`base`、`console-session` 与 `diagnostics`；verification
  consumer 只允许位于 `scripts/fixtures/`，不得进入产品 userspace topology。
- root workspace 必须显式 exclude `user/console-session`。该 crate 使用标准 `src/lib.rs`
  staticlib，经唯一 `build_rust_user_program` seam 与 musl CRT/libc 链接为动态 PIE。
- BusyBox init 只 respawn `/bin/console-session`、network service 与 UART recovery ash。
- `/bin/console-session` 是唯一 userspace DRM、evdev、PTY child、ANSI model、font、dirty
  renderer 与 resize transaction owner。禁止第二 display consumer、display broker、私有应用协议、
  JavaScript runtime 或 terminal model transport。
- 应用 Interface 只有 PTY、termios、ECMA-48/DEC terminal semantics 与 checked `TERM=liteos`
  terminfo。应用必须是普通 Linux process 或 Alpine APK，禁止 LiteOS 私有 SDK/manifest。

## State ownership

- `reactor` 唯一拥有 active DRM/input fd、PTY master/child、deadline 与 pending resize。
- `Model` 唯一拥有 primary/alternate screen、parser、cursor、palette、mode 与 dirty spans。
- `Display` 唯一拥有 active framebuffer/GEM mapping；candidate 在 commit 前不对外可见。
- resize 固定执行 query → prepare model/framebuffer → query confirmation → SETCRTC commit →
  model commit → `TIOCSWINSZ`。commit 前失败保留旧 mode，commit 后无法同步 PTY 必须 fail-stop。
- PTY child 以 `PR_SET_PDEATHSIG(SIGKILL)` 和 parent-race 复查绑定 owner；session exit 关闭
  master、SIGKILL 并 reap child，禁止遗留孤儿 shell。
- reactor 没有 render/resize/blink deadline 时必须无限阻塞；输入和 PTY 每轮有固定 work budget，
  禁止 busy polling。

## Source fence

`console-session` 的 crate/source 直接条目由 architecture checker 精确比对。所有 Rust/C
production source 都受 600 行硬上限约束；超限必须沿 owner/interface seam 拆分，禁止豁免。
