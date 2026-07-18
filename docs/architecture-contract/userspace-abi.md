# 用户态与 ABI 契约

## Owner

- `syscall-abi` 独占已接入 Linux/riscv64 number；dispatcher 独占 number-to-handler mapping。
- syscall module 独占 raw UAPI codec、user-copy 和 errno translation；领域 module 独占行为与状态。
- task loader 独占 pathname/script rewrite；memory ELF loader 独占 ELF plan、mapping、initial stack 与 rollback。
- rootfs builder 独占固定 package/key/cache 输入；产品 userspace 只保留一条 musl/BusyBox/console-session runtime。

## Interface

- 未接入 number 返回 `ENOSYS`；不得建立私有 number、错号转发、silent flag ignore 或 userspace compatibility shim。
- syscall matrix中的每个入口必须唯一归属一个领域文件，并明确 Complete/Partial、对象范围与已知缺口。
- Linux/riscv64 register convention、signal frame、ELF/TLS 与 hwprobe 留在当前 ABI/backend；generic process/memory 不依赖其 layout。
- userspace application 不得依赖 LiteOS 私有 runtime、init、device protocol 或第二条 rootfs path。

## Failure and cleanup

- exec 在 point of no return 前完成 source、ELF、stack 与 owner allocation；失败保持旧 image，提交后只允许新 image 或进程退出。
- ABI copyout 失败不得发布不可回收的 fd、timer、mapping、socket control 或 process identity；
  `recvmsg` 收到的 fd 必须等 name、control 和 msghdr metadata 全部 copyout 成功后整批发布。
- Linux `clone` 的 `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` 是例外的 best-effort store：
  Thread identity 先发布但保持 `New`，store fault 不回滚、不改成功返回；全部 store 尝试
  完成后才按 process job-control 原子转为 Ready/Stopped。并发 `exit_group` 已提交时，新
  child 在变为 Ready 前继承 kernel SIGKILL，parent-visible exit status 仍由首次
  group-exit owner 决定。
