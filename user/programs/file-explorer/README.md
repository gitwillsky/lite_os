# file-explorer

类型：程序，产物 `/bin/file-explorer`；启动位置、外壳动词与独立文件确认入口由 `main.rs` 解析。

共享 SDK 拥有列表/图标视图、多选、侧栏树展开、菜单、滚动与原地编辑；本程序只拥有路径、快照、历史和文件操作。
目录读取及复制/移动/删除在有界后台池执行，正常关闭等待已开始的文件副作用；部分成功不伪装成事务回滚。
文件复制/剪切与拖放经标准 Wayland selection/data-device 传递 URI 集合，可与桌面及其他资源管理器窗口互通。
复制/移动先规范化目标目录，防止符号链接入口绕过递归判断；原子创建和 renameat2 不覆盖已有条目。
回收站发现 home trash 与本地卷 trash：原生字节 URL 编码、当地删除时间、先独占创建 .trashinfo；同卷 rename 保留身份，失败保留源文件。
桌面删除和清空回收站使用同一文件事务及独立确认框，不创建浏览窗口。
依赖 liteos-ui、calendar、linux-uapi 和 desktop-entry；桌面风格参考见 [资源管理器设计](../../../docs/design/classic-desktop/explorer.md)。
