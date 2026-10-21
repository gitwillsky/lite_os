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
- **静态阅读的新线索（未证实，2026-10-20）**：`read_leaf` 先经 `read_metadata_block` 取块，再用本目录 inode 的 `i_generation` 派生 seed 校验；
  `lookup_entry` 等读者逐块 `map_block` → `read_leaf`，两步之间不持目录锁。若该目录同时被 rmdir，块被释放并重新分配给另一个目录，
  读到的块带有合法 tail 但 seed 不同，表现正好是 “tail/checksum mismatch”。这与“写者暂存未完成”是两种不同的竞态，
  复现后需先区分：失败时 `directory 203` 是否已被 unlink（`i_links_count == 0`）、`leaf 2447` 是否在失败前被释放过。
- **复现尝试（2026-10-21，修复前基线）**：强制重跑 `make verify-runtime-busybox`（fsstress 所在启动为 `-smp 8`，aarch64/hvf，12 物理核宿主）共 93 次
  （前 13 次 + 后 80 次），`tail/checksum mismatch` 出现 0 次。期间另有 2 次失败与目录无关：1 次是我留下的孤儿进程占用 18080 端口造成的污染（作废），
  1 次是 `LITEOS_UNLINK_RELEASE` 的 `df` 空间差值检查超时（另一个未调查的间歇问题）。结论：整门禁循环对该竞态不敏感，0 次不能证明它不存在。
- **现场诊断（2026-10-21 已加，只读、不改变行为）**：`read_leaf` 校验失败时一次性记录现场（`directory_block.rs::report_leaf_mismatch`）。解读：
  - `fresh_read=valid` 且 `cached_image_unchanged=Some(false)`：刚才那份是撕裂/过期 image，属读写竞态；
  - `fresh_read=invalid` 且 `mapped_at_logical=None`（或 `links=0`）：读者拿着过期映射读到了被释放的块，属 rmdir 竞态；
  - `fresh_read=invalid` 且映射仍指向同一块：块内容本身就不是这个目录的 leaf（磁盘损坏或写入路径产生了坏块）。
  没有加 panic：校验和失败也可能来自真实磁盘损坏，生产路径 panic 会让坏镜像使整机停机；需要冻结现场时再加只在门禁里启用的内核参数。
  `LITEOS_UNLINK_RELEASE` 的两条检查失败时会多打一行 `LITEOS_UNLINK_RELEASE_DIAG ... b=.. m=.. a=..`，给出各步的空闲块数。
- **下一步**：
  1. 写一个只跑 fsstress（`scan` + `dir_race` + `link_race`）的小循环（整门禁重跑已证明太不敏感，需要专门加压 `dir_race` 与 `scan`），`-smp 8` 下反复跑并统计复现率，作为修复前后的度量。
  2. 沿 `read_leaf` 的调用链确认目录读者是否持有目录锁、读到的块来自 journal 暂存还是已提交盘面，以及 `write_leaf` 暂存的可见性边界。
  3. 若确认是读写竞态，修复后把复现用例固化进门禁，并在 `docs/architecture-contract/filesystem-storage.md` 记录目录读写的互斥与可见性契约。
- **不要做的事**：不要用重跑、清缓存或放宽围栏让门禁“通过”来代替定位；也不要在没有复现数据时直接改 ext4 目录锁。
