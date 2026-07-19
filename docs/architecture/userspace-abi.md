# 用户态与 ABI 当前架构

## 当前设计

- kernel 只暴露固定 Linux/riscv64 UAPI 子集。syscall dispatcher 使用共享编号 crate，未接入编号返回 `ENOSYS`。
- ELF loader 支持当前声明的 RV64 ET_EXEC、动态 PIE、PT_INTERP、TLS、RELRO、auxv 与 Linux script rewrite；filesystem 只提供 executable source seam，memory 拥有映射与 initial stack。
- 产品 userspace 是固定 musl runtime、BusyBox `init + ash`、dependency-free Rust `console-session` 和单 ELF `liteos-stress` diagnostics。
- write/send 的 stack/heap staging 统一由 `UserInputStaging` 管理 initialized prefix，memory copyin 直接写未初始化 storage。两条 64KiB socket staging 加一条 128KiB regular staging 的预清零成本从 262,144B 降为 0。
- rootfs 由固定 Alpine package/key 输入构造；应用与 terminal 只通过标准 Linux process、fd、PTY、termios、socket 和 ELF ABI 交互。

## Known limits

- 支持矩阵只证明列出的 syscall、对象类型和 consumer，不宣称完整 Linux、POSIX 或任意 musl 程序兼容。
- Linux/riscv64 calling convention、signal frame 与 hwprobe 是当前 backend ABI；其他 architecture 必须定义自己的 userspace ABI backend 和验证矩阵。
