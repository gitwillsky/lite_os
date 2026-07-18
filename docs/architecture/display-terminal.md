# Display、input 与 terminal 当前架构

## Kernel

VirtIO-GPU 经 DRM primary node `/dev/dri/card0` 提供 dumb buffer、legacy KMS、DIRTYFB 与
hotplug uevent。VirtIO-input 经 `/dev/input/eventN` 提供 evdev keyboard/tablet；Unix98 PTY、
termios、foreground process group 与 signal/job-control 走标准 Linux ABI。

## Userspace

`/bin/console-session` 是单线程 `no_std` Rust executable，也是唯一图形 console Module：

1. 打开 DRM、订阅 netlink hotplug并取得 checked terminal atlas；
2. 创建 framebuffer、回放 boot log并完整绘制初始 terminal grid；
3. 创建 PTY，在 child session 中执行 `/bin/sh`；
4. 用一次 blocking `poll` 同时等待 PTY、hotplug、keyboard、pointer 与 frame/blink deadline；
5. 只重绘 dirty cell span，并通过 DIRTYFB 提交有界 clip；
6. resize 以候选 model/framebuffer 事务提交，成功后向 PTY 发布真实 pixel/cell winsize。

应用不连接 console socket，也不发布 scene。shell、vim、htop、tmux 等程序只看到
`TERM=liteos`、PTY、termios 和标准 signal/process/filesystem/network ABI。

## Data flow

```text
evdev ──> reactor ──> bounded input queue ──> PTY master ──> shell/TUI
shell/TUI ──> PTY master ──> Model ──> dirty spans ──> Display ──> DRM
netlink hotplug ──> reactor ──> candidate Model + framebuffer ──> KMS commit ──> TIOCSWINSZ
```

`reactor` 只编排事件和 transaction，不复制 terminal/display 状态；`Model` 与 `Display`
分别是字符语义和 scanout 资源的唯一事实源。三个方向都不经过第二个 broker、transport 或缓存。

## Rootfs topology

```text
BusyBox init
├── /bin/console-session
│   └── /bin/sh (PTY controlling terminal)
├── /etc/init.d/network-service
└── -/bin/sh (UART recovery console)
```

rootfs 不包含 display broker、compositor、window manager、QuickJS、UI SDK、私有协议、应用目录
或第二套 package profile。图形 console crash 由 init 重新启动；UART recovery path 不依赖图形状态。

## Known limits

当前 terminal model 覆盖 UTF-8、DEC/ECMA CSI、滚动区、alternate screen、16/256/truecolor、
应用光标/键盘、X10/VT200 mouse、blink 与 primary reflow。尚未声明完整 grapheme cluster、SGR
mouse 1006、bracketed paste、OSC 8/52、Kitty keyboard 或 synchronized output 支持。
