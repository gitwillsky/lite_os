# Phase 60：单轨图形 console session

实现状态：完成。

本阶段把多进程桌面栈收敛为唯一 `/bin/console-session`，以标准 PTY/termios/ANSI 作为应用
Interface。显示、输入、终端状态和渲染在进程内闭合，不再使用 display broker、窗口协议或
JavaScript application runtime。

完成条件：

- init 直接监督 console、network 与 UART recovery 三个独立 action；
- console 独占 DRM/evdev并在 PTY 内启动 shell；
- resize failure-atomic，PTY winsize 与 committed mode 同步；
- session exit 不遗留 PTY child；
- rootfs 只接受普通 Linux executable/APK，不存在私有应用 ABI；
- idle reactor 无限阻塞，源码文件受 600 行硬围栏。
