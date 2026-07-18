# 设备与终端当前架构

## 当前设计

- `platform` 发现并装配具体 adapter；`drivers` 只公开 block、network、display、input、RTC、RNG 与 interrupt 等通用 seam。
- VirtIO queue、DMA slot、descriptor ownership、completion 与 reset 状态由各设备 adapter 单独拥有；hardirq 不分配、不阻塞。
- DRM owner 组合 display operation、GEM/framebuffer、KMS、damage fence、master 与 event；syscall 只编码 Linux DRM UAPI。
- input owner 组合 device state、每-open evdev queue、grab、clock 与 revoke；VirtIO input adapter 只提供 raw event/config。
- PTY registry、pair、Terminal session/foreground/winsize 与 Rust `console-session` 各守自己的 seam；控制面使用标准 PTY、termios、ANSI/ECMA-48。
- terminal font 是 checked A8 atlas；普通构建只消费生成产物，升级由显式 generator 完成。

## Known limits

- DRM atomic/auth/lease、完整 evdev output/multitouch 和设备热拔插尚未开放。
- 图形 terminal 是当前固定 userspace consumer，不代表通用 GUI stack。
