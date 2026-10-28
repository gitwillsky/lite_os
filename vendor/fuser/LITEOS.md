# LiteOS FUSE consumer dependency

唯一编译来源是本目录，源自 crates.io fuser 0.17.0（MIT）。原始文件摘要见
`UPSTREAM.sha256.json`，原始 manifest 保留于 `Cargo.toml.orig`；未使用的测试与示例及其
target/dev-dependency 声明不打包。

`src/mnt/fuse_pure.rs`：Linux 直接挂载将 `Subtype` 编为标准 `fuse.<subtype>`
文件系统类型，独立保留 `FSName` 源标签。否则 root 直接挂载只显示 `fuse`，依赖真实挂载类型的
网络盘识别与清理会失败；普通用户 helper 路径保持上游语义。

`src/ll/fuse_abi.rs`：删除未被任何编解码路径使用的 CUSE、ioctl retry iovec 与 notify retrieve
私有布局定义，保持本地依赖在 `-D warnings` 下的构建约束；现有请求处理与 wire ABI 不变。

新增字符串分配只在挂载冷路径发生，不进入 FUSE 请求处理热路径；无需新增 benchmark。
