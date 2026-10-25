# file-explorer

类型：程序，产物 `/bin/file-explorer [位置] [--select 名称] [--verb delete|rename|properties]`。

共享 SDK 拥有列表/图标视图、多选、侧栏树展开、菜单、滚动与原地编辑；本程序只拥有路径、快照、历史和文件操作。
目录读取及复制/移动/删除在有界后台池执行，正常关闭等待已开始的文件副作用；部分成功不伪装成事务回滚。
复制/剪切的路径集合当前只在本进程有效，尚未接入跨进程文件剪贴板。
复制/移动先规范化目标目录，防止符号链接入口绕过递归判断；原子创建和 renameat2 不覆盖已有条目。
回收站支持 home trash：原生字节 URL 编码、当地删除时间、先独占创建 .trashinfo；跨卷删除失败保留源文件。
依赖 liteos-ui、calendar、linux-uapi 和 desktop-entry；桌面风格参考见 [资源管理器设计](../../../docs/design/classic-desktop/explorer.md)。
