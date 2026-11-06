# savedump

类型：程序，产物 `/bin/savedump`；依赖 liteos-ui。

对应 Windows 2000 的 `savedump.exe`：桌面外壳在会话开始时启动它。上一次启动以内核 panic 结束时
（`/proc/last_panic` 非空），显示“系统已从严重错误中恢复”对话框并附上记录的首行；同一次启动只提示一次
（`/tmp` 中的标记文件，随 tmpfs 在下次启动时消失）。没有记录时直接退出。
