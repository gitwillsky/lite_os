# terminal

类型：程序，产物 `/bin/terminal [程序 参数...]`。

使用 liteos-ui Canvas 和既有 terminal-model，标准 PTY/session/process-group 由 linux-uapi 拥有。
默认启动当前账户 `/bin/sh`，TERM=linux；输入和输出使用非阻塞 FD readiness，关闭在后台终止并回收进程组。
Ctrl+Shift+C/V 复制/粘贴，滚轮浏览历史；不在程序内实现标准窗口控件或直接提交像素。
当前实现未进行 GUI/guest 运行验收。
