# Desktop milestone 1

## Required vertical slice

- `make run-gui` 从图形 recovery frame 进入 SolidJS 开机动画，再自动进入无登录 desktop。
- Classic theme：LiteOS 品牌的 Windows 2000 风格背景、taskbar、start menu、clock、desktop icon
  与窗口装饰。
- window create/focus/z-order/move/resize/minimize/maximize/restore/close 全部闭环。
- QEMU host resize 不黑屏、不丢 session；pointer hot path 不触发 full-frame redraw。
- Terminal 作为 `terminal-service + TextGrid + Solid window` 运行；Calculator 作为纯 Solid 应用运行。
- Shell/application OOM 或 crash 不影响 compositor；compositor generation failure 可确定重建。

## Explicit non-goals

第一阶段不实现 login/multi-user、file manager、browser、network JS API、audio、3D/GPU backend、
完整 DOM/CSS、IME、clipboard 或 session restore。UART ash 始终作为 recovery console 保留。

## Performance gates

- idle reactor 无限阻塞，稳定 idle CPU 接近零。
- pointer 到可见 cursor 不跨越一个 refresh interval。
- 每 client 每 refresh 最多一个 UI transaction、一个 visible frame commit。
- Shell QuickJS heap `<= 8 MiB`，普通应用默认 `<= 4 MiB`。
- UI scene state 不含 scanout buffer `<= 16 MiB`；1920x1080 双 buffer 单独计约 16 MiB。
- resize、应用 OOM、Shell restart 与 compositor restart 均不得产生无法解释的资源残留。
