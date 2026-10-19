# TODO

待处理事项。每条写清现象、证据、当前判断和下一步；处理完成后迁移持久事实到 `docs/` 并删除条目。

## ext4 目录叶子块校验和间歇性失败（并发读写竞态）

- **状态**：未处理，间歇性，尚未稳定复现。记录于 2026-10-20。
- **现象**：BusyBox 运行时门禁（`make verify-runtime-busybox`，`-smp 8`）的 fsstress 阶段偶发内核错误日志
  `[ERROR] [kernel::fs::ext4::directory_block] directory 203 leaf 2447 tail/checksum mismatch`，随后该门禁失败。
- **证据**：
  - 来源是 `kernel/src/fs/ext4/directory_block.rs` 的 `read_leaf`：读出叶子块后校验 tail 与 checksum，不一致即返回 `InvalidFileSystem`。
  - 错误出现在 fsstress 的 `scan` 阶段，即一边扫描目录、一边其他线程在 `dir_race`/`link_race`/`churn` 中修改同一目录的时候。
  - 同一门禁连续 5 次真实运行（1 次完整 `make verify` 内、1 次单独重跑、3 次清缓存强制重跑）中失败 1 次，其余 4 次通过且无任何校验和日志。
  - 失败发生时 `fs/ext4` 没有任何未提交改动，所以它不是 io_uring 切片引入的逻辑错误。
- **当前判断（未证实）**：目录叶子块的读取路径与写入路径（`write_leaf` 暂存 tail/checksum）之间缺少应有的互斥或快照一致性，
  读者可能看到写者尚未完成的块。io_uring 一轮新增的 `io_uring` 内核线程与读写核心重构只改变了 8 核下的时序，更容易碰到它；
  没有证据表明它在此之前就不存在，也没有证据表明它被这些改动放大。
- **下一步**：
  1. 写一个只跑 fsstress（`scan` + `dir_race` + `link_race`）的小循环，`-smp 8` 下反复跑并统计复现率，作为修复前后的度量。
  2. 沿 `read_leaf` 的调用链确认目录读者是否持有目录锁、读到的块来自 journal 暂存还是已提交盘面，以及 `write_leaf` 暂存的可见性边界。
  3. 若确认是读写竞态，修复后把复现用例固化进门禁，并在 `docs/architecture-contract/filesystem-storage.md` 记录目录读写的互斥与可见性契约。
- **不要做的事**：不要用重跑、清缓存或放宽围栏让门禁“通过”来代替定位；也不要在没有复现数据时直接改 ext4 目录锁。
