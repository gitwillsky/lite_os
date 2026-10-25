# notepad

类型：程序，产物 `/bin/notepad [文件]`；依赖 liteos-ui 和 calendar。

记事本只拥有路径、保存 revision 与未保存决策；共享 TextEditor 拥有 UTF-8 文档、光标、选区、Tab 和双向滚动。
打开、保存和保存确认使用共同异步对话框，有界后台池执行文件 I/O；写入期间继续编辑的内容保持未保存状态。
打开限制为 1 MiB，按 UTF-8 读取（无效字节替换）并以 LF 保存；没有撤销、查找和自动换行。
