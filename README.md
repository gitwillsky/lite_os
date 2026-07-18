# LiteOS

LiteOS 是一个以 Rust `no_std` 实现、面向多架构演进的紧凑型操作系统内核。通用内核通过编译期静态 `arch` 与 `platform` façade 消费硬件能力；当前唯一可用 backend 是 RISC-V64，当前唯一支持的 machine 是 QEMU `virt`。

项目追求清晰的状态所有权、窄接口、可证明的错误与清理路径，以及可持续执行的单元、性能和运行时验证。没有实现的能力不会通过私有 ABI、兼容入口或双轨实现伪装为已支持。

## 快速开始

准备 `qemu-system-riscv64` 后执行：

```bash
make build
make run
```

构建环境、测试入口和缓存规则见 [构建与验证](docs/development/build-and-verify.md)。架构、接口、ABI 与规范资料从 [文档索引](docs/README.md) 进入。
