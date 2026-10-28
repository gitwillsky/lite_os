# LiteOS SMB dependency

唯一编译来源是本目录，源自 crates.io smb 0.12.1（MIT）。原始文件 SHA-256 见 UPSTREAM.sha256.json，
原始 Cargo manifest 保留于 Cargo.toml.orig。未使用测试/示例不打包，manifest 只去掉其 target/dev-dependency 声明。

本地语义修复仅限 src/resource/file.rs：READ 不用 CREATE 的旧 EOF 提前结束，标准 STATUS_END_OF_FILE 映射零字节；
GetLen 用 QUERY_INFO 的实际 FileStandardInformation 长度。保护新建后写入、同句柄重读与远端扩展；不新增协议或认证路径。
产品只选择 multi_threaded、sign、AES-128-CCM/GCM，不启用 SMB1、Kerberos、NetBIOS、QUIC、压缩或 multichannel。

最窄 feature 构建修复：encrypt_ccm.rs 的 AES-128/256 类型别名各随其实际 feature 编译，避免只启用 AES-128 时引用未导入 Aes256。

- GCM 类型别名与对应 AES feature 一致；file_util 的异步辅助导入只在 async 下启用，避免未启用功能的孤儿条目。

- 文件 I/O 保留结构化 SMB 错误（不转字符串），READ/WRITE 回复长度不允许超过原缓冲；服务器畸形回复不能触发切片 panic 或虚报完成字节。

- Client::close 的 TREE_DISCONNECT 失败仍清理所有 session/transport，最后返回首个错误；网络中断不能跳过实际连接关闭。
