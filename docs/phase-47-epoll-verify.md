# Phase 47：epoll 正确性与 verify 增量执行

本阶段不增加私有 ABI，也不建立第二套 wait registry。目标是修复 Phase 46 已确认的 epoll 生命周期/并发偏差，并在不减少验证覆盖的前提下降低相同输入重复执行 `make verify` 的成本。

## epoll owner 与不变量

- `OpenFileDescription::descriptor_refs` 是跨 fork 后独立 fd table 的 descriptor 引用 owner。计数从一降到零时，weak epoll registry 删除所有指向该 OFD 的 interest；epoll 自身持有的 Arc 不会阻止 Linux 最后 descriptor close 语义。
- interest identity 是 `(descriptor number, OFD identity)`。已注册 fd 被关闭但 OFD 仍由 dup 保活后，复用相同数字的新 OFD 可以独立 ADD；MOD/DEL 只命中当前 fd 解析出的同一 OFD。
- Pipe、AF_UNIX 和 Terminal 从单一全局 allocator 取得 readiness generation。ET 消费 generation，而不是比较 ready 位图；数据在保持 ready 的期间再次到达仍可形成新 edge。
- snapshot 带 MOD revision。完整 RV64 `epoll_event[]` 单次 copyout 成功后才提交 ET/ONESHOT；并发 MOD/DEL 或 fd reuse 令旧 revision 失效，旧 delivery 不覆盖新 state。
- 成功 delivery 推进唯一 `(fd,OFD)` cursor，下一次 snapshot 从其后轮转；`maxevents` 小于 ready 数量时，永久 LT-ready 的低 key 不会饿死后续 interest。
- 每个 epoll 只有一对内部 notification Pipe endpoints，并复用 IndexedWaitQueue 的 Pipe key；并发 ADD/MOD/DEL 或最后 close 会唤醒基于旧 interest snapshot 睡眠的 waiter，重新求值前统一排空 notification，不建立第二套 registry。
- 嵌套图变更由单一 graph lock 串行化，拒绝 self-loop、cycle 和超过 5 层的图；readiness 与 wait-key 递归因此有界。

Phase 47 当时尚未实现 `EPOLLEXCLUSIVE`，因此没有以 wake-all 冒充；该缺口已由 Phase 48 在同一 IndexedWaitQueue 内完成。

## verify 成本模型

- Clippy 已完成同一 crate 的类型检查，`verify` 不再紧接着执行等价 `cargo check`。
- musl、BusyBox 和 boot-topology gate 的 success stamp 对 kernel、bootloader、rootfs 的 ELF/library/recipe 输入、gate script、QEMU helper、QEMU 绝对路径与版本做 SHA-256/identity 绑定。ext2 每次格式化产生的时间/UUID 不属于语义键；所需 image artifact 不存在时仍强制重建。只有全部语义输入一致且上次完整成功才命中，失败或中途退出不会发布 stamp。
- hart 覆盖仍为 1/3/8：musl 固定覆盖 1，BusyBox 覆盖 1/8，独立 boot gate 只执行其唯一新增覆盖 3。
- UART 输入按 4-byte chunk、1 ms 间隔注入，仍保留 trigger settle；这只降低 PTY 系统调用和固定 sleep 数量，不改变 marker、timeout 或 crash-recovery gate。
- `LITEOS_VERIFY_REBUILD=1 make verify` 绕过 runtime success stamp，构建产物自身仍按各自 content fingerprint 决定是否重建。
