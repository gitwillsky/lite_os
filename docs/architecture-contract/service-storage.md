# 服务私有存储契约

本域由[文件系统与存储契约](filesystem-storage.md)进入；只描述服务私有数据能力，不拥有路由配置或备份策略。

## Owner 与能力

fshost 的 DataVolume 是原生数据卷唯一 FUSE 客户端，StorageService 能力只由根服务派生给对应服务。
服务名只用于派生目录，已部署的稳定存储身份不能因进程重命名而变更。每个 ServiceStorage 拥有一个目录 LOOKUP，最后一个连接退休时 FORGET。
Storage client 可观察启动 channel 的 PEER_CLOSED；能力永久关闭后必须由组件生命周期重新派生，不能周期重试失效句柄。

## 控制与内容

私有存储控制帧版本 1；read/write/create 正文一律通过独立只读 VMO，逻辑长度为 u64，空文件与不存在分别表达。
不以 channel 容量设文件上限，不保留内联写入旁路。接收先检查版本、包尾、名称/能力数与对象边界，再复制独占内容；分配/I/O 失败不发布部分内容。
方法序号：READ(1)、WRITE(2)、REMOVE(3)、SYNC(4)、LIST(5)、CREATE(6)。每次请求/回复均以 u32 私有存储版本开头。
READ/WRITE/CREATE/REMOVE 带经 storage_name 检查的 bytes 名字；WRITE/CREATE 再带 u64 逻辑长度和一个内容 VMO。
READ 存在时回复 u8=1、u64 长度及一个 VMO，不存在仅回复 u8=0；其余修改成功仅回复版本且无句柄。
LIST 无额外请求字段；回复 u64 内容长度及一个 VMO，正文为 u64 项数后逐项 bytes 名字、u64 大小、i64 UTC 修改秒数。

LIST 只枚举合法直接普通子文件，不包含目录、`.`/`..` 或不符合名字契约的内部项。DataVolume 的 READDIR/LOOKUP/RELEASEDIR/FORGET 逐项归还已取得资源；
目录本身的长期引用由能力拥有，不能在一次枚举结束时 FORGET。FUSE dirent 字节检查归 filesystem-protocol::directory，与 linuxd 共用，
完整检查名称和 8-byte padding；进展 cookie 是不透明值，只检查未进展，不假设数值递增。大小和时刻来自实际 FUSE attr，不由配置服务猜测。

## 原子可见性与持久化

DataVolume 按 FUSE CHUNK 渐进读取并检查实际长度/分配失败，正常 EOF 后归还打开与 lookup 资源。
write 独占创建带不复用身份的临时文件，按 FUSE WRITE 实际完成量推进；零进展、超报或畸形回复拒绝，短写继续。
全部字节写完并 FSYNC 后才 RENAME，再用 FSYNCDIR 确认命名状态；失败清理本次独占文件，不覆盖残留或另一连接的在途文件。
FUSE 请求/临时文件共用既有原子身份分配器，耗尽返回错误而不回绕，不新增锁或全局表。
remove 在 UNLINK 或 ENOENT 后均 FSYNCDIR；重试未确认删除不能仅凭文件已不在就声称持久化。sync 只确认目录，不重写内容。
写入/删除返回错误可能已改变命名状态，调用者必须拥有提交决定；原子可见性不等于失败时保证未修改。
配置 owner 可比较原字节后同步目录，或完整写回/删除来撤销；原字节相等的快路径以该服务为唯一配置写者、旧内容已持久化为前提。
这套低频配置能力不经过 Linux VFS，不改普通文件读写/共享 VMO 数据面。短写/清理已作源码审查，真实 ext4 EIO/断电验收仍待统一阶段。

create 复用同一临时写入/FSYNC/发布路径，唯一差异是 RENAME2_NOREPLACE：已有目标返回 ALREADY_BOUND，不覆盖原内容。
名字冲突或回复不确定时，调用者只能比较原文件/同步确认，不能切换为覆盖写。归档身份、保留数量及重试决定属于使用此能力的备份 owner。
