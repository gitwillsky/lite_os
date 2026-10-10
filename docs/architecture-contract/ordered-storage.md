# 有序存储与源码审查

### Fallible ordered-storage contract


- 每个 AVL node 的 `next` 是唯一 in-order successor link；所有 structural mutation 在独占
  map borrow 内同步维护它，split 清除跨 map link、join 恢复 boundary link、retain 重建全部
  links。`Iter` 的 immutable map lifetime 保证 raw successor target 在解引用期间存活，iterator
  因而只有一个 pointer（RV64 ≤ 16 bytes）且不清零 path stack；遗漏任一 mutation 的 link 更新
  会造成漏项或悬垂指针，必须由 topology/link invariant test fail-stop。production 单邻居查询使用
  floor/ceiling/predecessor/successor，禁止用 `iter_from/iter_after(...).next()` 重建 iterator。
  map 的 unsafe Send/Sync 只恢复 `K/V: Send/Sync` 的结构 auto-trait：跨线程移动不会移动 Box
  pointee，共享访问不能改变 links；`VacantEntry` 发布前/摘除后恒为 `next=None`，`NodeSlot`
  尚未初始化，二者只按 K/V 恢复 Send/Sync。缺失这些证明会把内部 NonNull 错误泄漏为非
  Send owner，放宽到不满足 K/V bound 则会跨线程移动领域数据。
- `split_off` 只沿 AVL root-to-leaf path 比较并重组现有节点，再对移出的 subtree 做一次无 key comparison 的精确 cardinality 遍历；不为所有节点增加永久 subtree-size 字段，复杂度为 `O(log n + k)`。
- bulk join 只接受 `left.max < right.min` 的 ordered-disjoint 输入，边界检查必须在 mutation 前完成；重复或逆序表示 caller contract 损坏并 fail-stop，两表保持不变。join 只改动 AVL spine，复杂度为 `O(log n)`。
- `retain` 消费原 topology 后只执行一次 node ownership filter 与一次 sorted-list balanced rebuild；每个节点访问常数次、无分配、保留节点 identity，禁止逐节点 `join_*` 双轨。
- regression 的 primary metric 是 AVL order/height/length/successor-link/model 不变量、RV64 iterator ≤ 16 bytes、完整有序扫描至多一次 key comparison，以及 split 不超过原树高的 key comparison、join 一次边界 comparison 和 cardinality 每个移出节点
  一次 structural visit。
  host wall time 会混入建树 allocation 与 runner 调度，不作为稳定 gate；只有新增持续调用该 bulk interface 的 production 热路径时才增加 wall-clock benchmark。

## State-owner registry

每个复合状态只能由一个领域 owner 持有。精确 owner 在对应子契约维护；下表由 checker 读取，必须与 production source 双向一致。

## 源码审查

生产 kernel/bootloader Rust 源文件、`tools/kernel-unit/src` 和 `tools/scheduler-unit/src` 的 host unit-test Rust，以及 architecture-check 的 `_tests.rs` module 均适用两级围栏：
超过 1000 行触发 review notice，但不单独导致验证失败；超过 1500 行默认拒绝。
architecture-check unit-test module 必须使用 `_tests.rs` 后缀，禁止把测试正文塞回 production `main.rs` 绕过围栏。
reviewer 必须检查 owner、依赖方向、公开接口与真实领域 seam。生产 source 可以拆成深
module，或在下表登记精确审查额度；登记必须给出 owner、暂不拆分的原因与消除条件。
额度是硬上限，只能随重构下降，不得为功能开发上调；文件缩短时 checker 强制同步下调。
行数只是退化信号，禁止按行数机械切片或建立 pass-through module。

host unit-test 入口只装配 production path module 与领域测试 module；测试正文按 filesystem/storage、memory、platform/execution、socket/ABI 等真实 seam 放入独立文件。
unit-test source 的 review notice 不允许把新测试继续堆进入口文件；必须先检查测试所属 owner 并迁移到对应领域 module，或新增有领域含义的 module。

`user/` 下项目自有 Rust/C/header/JS/TypeScript/CSS source 采用相同的 1000 行 review、1500 行
默认拒绝两级围栏。review 必须检查状态 owner/interface，不能仅为降行数建立 pass-through module；
checker 递归扫描实际文件集，避免新 crate、应用或 native bridge 绕过围栏。唯一排除目录是
`user/quickjs-runtime/vendor/quickjs/`：它必须保持 standards baseline 固定官方 tarball 的原始文件，不能承载项目 patch；项目 C adapter 仍在 `src/`
接受 1000/1500 两级围栏。缺少该精确排除会把不可拆分的上游实现误判成项目 module，扩大到其他 vendor
目录则会形成逃逸围栏。

## 控制缓冲区资源边

packet SendMemoryState 的额度与 usage 唯一由 UnixSocket.send_memory 拥有；SenderCharge 引用实际 sender 并在 transport/GRAPH guards 外退款。VacantEntry::allocation_size 只返回 actual Node layout 供额度计算，不暴露 topology 或新的 mutator；
ReceiveBuffer 的用户 limit/真实 capacity 由原 buffer owner 保持，deferred reserve 不创建副本 transport。上述新 scoped interfaces 与生成基线将在全部实现结束后统一更新及验证。

## 当前源码大小审查

- `kernel/src/storage/fs/procfs.rs`：owner 为 procfs inode/source 投影。开放文件操作已拆入 opened_operations，仍集中维护 ProcNode 的 Exhaustive 匹配以避免重复目录事实；新增独立目录 family 时移入对应 source adapter，达到 1500 行前必须拆分。
- `kernel/src/storage/fs/tmpfs.rs`：owner 为单个 tmpfs inode 与 namespace 生命周期，数据块由 TmpFile 独立持有。现有 inode trait 实现保持同一权限/通知事务；新增独立 metadata family 时拆分，达到 1500 行前必须拆分。
- `kernel/src/task/process_table/signal.rs`：owner 为 graph 下的信号生成与子进程通知事务，凭据/queue codec 已分属其他 owner。退出通知与 pending admission 必须同锁裁决；新增独立 signal producer 时拆分，达到 1500 行前必须拆分。

上述为当前 seam 复核记录，源码仍受原 1000 行复核与 1500 行拒绝门禁约束。
