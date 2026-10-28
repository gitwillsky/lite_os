# smb-fscc 0.12.1

来源：https://crates.io/crates/smb-fscc/0.12.1 ，原文件摘要见 UPSTREAM.sha256.json。
唯一修改：FileRenameInformation 的计算长度字段加 br(temp)，与同文件其余计算长度字段一致，使公开类型可以构造；线上编码不变。移除上游测试、示例及其构建依赖。
