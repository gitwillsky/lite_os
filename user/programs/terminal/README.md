# terminal

`/bin/terminal [程序 参数...]` 默认从当前账户的 HOME 启动 `/bin/sh`，提供 `TERM=xterm-256color` 与 `COLORTERM=truecolor`。
VT 与回滚采用 Alacritty，显示使用共享 Canvas 和系统等宽字体。支持备用屏幕、应用键盘、鼠标/焦点报告、括号粘贴、
同步更新、窗口缩放与 OSC 颜色/标题。剪贴板由桌面快捷键控制，OSC 52 禁用。

Ctrl+Shift+C/V 复制和粘贴。TUI 开启鼠标报告时按住 Shift 选择文字或滚动历史。关闭窗口会终止并回收 PTY 子进程组。
