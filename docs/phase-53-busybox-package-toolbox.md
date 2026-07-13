# Phase 53：BusyBox 包获取与归档工具箱

本阶段在唯一动态 BusyBox 1.37.0 rootfs 上开放 `tar/unzip`、bzip2、XZ 解压以及 `cmp/du/xargs/env/which/readlink/realpath`，与已有 `wget/sha256sum/gzip` 组成下载、校验、解包和 shell 编排链。所有入口仍是 `/bin/init` 同一 inode 的 hardlink，不引入独立工具或 BusyBox source patch。

## 实现边界

- `tar` 支持创建、gzip/bzip2/XZ seamless 解包、GNU 长文件名、include/exclude、`--to-command`、软硬链接和 owner/mode 元数据。
- bzip2 支持压缩与解压；BusyBox 上游 `xz` applet 仅提供解压，因此不声明 XZ 压缩。`unzip` 消费标准 central-directory/deflate ZIP。
- VFS `create_at` 对 `/`、`.` 与 `..` 返回 `EEXIST`；仍先解析 parent，因此 `/missing/..` 保持 `ENOENT`。这是 Linux namespace errno 语义，不是 tar 特判。

## 运行验收事实

- guest 创建并解开 tar.gz，核对超过 ustar name field 的文件名、payload、`0640` mode、symlink target 与 hardlink inode identity。
- guest 完成 bzip2 round-trip，并解压 Python 标准库生成的 XZ/ZIP fixture。
- 含 `../../phase53-escape` 的 tar fixture 不得在目标目录外创建文件。`xargs/env/which/du/readlink/realpath/cmp` 均经真实 ash 调用链执行。
