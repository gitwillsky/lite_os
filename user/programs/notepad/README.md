# notepad

- **类型**：程序（可执行文件）
- **作用**：记事本：纯文本的查看与编辑
- **产物 / 安装位置**：`/bin/notepad [文件]`
- **对应的 Windows 2000 组件**：notepad.exe
- **依赖**：classic-ui、classic-theme、canvas、typeface、liteos-runtime

菜单栏（文件、编辑、帮助）、多行编辑区（光标、选区、鼠标点选与拖选、竖向滚动条、Tab 按 8 列显示）、
未保存修改的"是/否/取消"确认。新建、打开、另存为用单行输入框键入路径。剪贴板只在本进程内
（系统还没有 `wl_data_device`）；没有撤消、查找、自动换行与横向滚动条，文件按 UTF-8 读写、换行用 `\n`，
超过 1 MiB 的文件拒绝打开。资源管理器按扩展名经桌面条目 `MimeType=text/plain` 把文本文件交给它。
