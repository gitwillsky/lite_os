# 内存契约

## Owner 与接口

| Owner | 唯一状态与接口 |
|---|---|
| `kernel::memory` | 物理页分配/归还、分配级别、保留 boot 区间与内核堆 |
| `kernel::vm::vmo` | COW 树、页内容、pin、pager 请求/脏页及映射的 Weak 登记 |
| `kernel::vm::address_space` | 每个地址空间的 VMAR 树、页表、Mapping publication 与共享成员 |
| `kernel::vm::translation` | PTE mutation 的 fence 与待退休页表 owner |
| `kernel::vm::device_space` | 设备页表及等待 IOMMU 失效的 retirement token |
| `linuxd::memory::mm` | Linux VMA/heap/stack 与 native 映射的一致事务 |
| `linuxd::fs::page_cache` | 文件 pager、后端读取与回写策略 |

通用 kernel 不解释 mmap flags、RLIMIT、文件偏移、Linux errno 或进程身份。Linux syscall 只解码 UAPI、
校验用户输入并交给 linuxd 的相应 owner。应用不调用 native ABI。

## COW 树与生命周期

1. 同一 COW 树的 topology、已提交页和 pager 请求由一把 task mutex 串行。VMO 内的短锁只承载 Rust
   同步访问；普通状态访问须持树锁，`size` 的单字段投影和 pager 完成观察只取状态短锁。
2. Relation 的强引用只沿子到父；children 和 Mapping registry 只用 Weak。快照把源在原父对象中的
   Weak 登记替换为新隐藏节点，并登记源/快照为该隐藏节点的两个直接子对象，不能遗留旧 topology。
3. 只剩一个存活子对象的隐藏节点可合并进该对象。普通访问折叠直接祖先；快照慢路径遍历祖先，
   使通知退出与映射退休分离时，中间节点同样被回收。Drop 不取树锁，避免树锁内的最后引用析构死锁。
4. 合并按偏移与可见窗口求交；子对象已有页覆盖祖先页。转移页与索引节点不分配，页框身份不变。
   缩短后的继承窗口不能在重新增长或再次合并时扩大。
5. 覆盖、decommit、resize 或 pager supply 必须先撤销对应对象及依赖子对象的翻译并完成全部 fence，
   再释放不再可见的页框。pin 期间不得撤销/替换页；不能冻结仍被 DMA 固定的页。
6. 分配失败不能发布半个 topology：隐藏节点、子节点及 children 容量在移交已提交页与 Relation 前准备。
   快照写保护失败仍须保留所有内容 owner，不能让已有翻译指向已释放页。

COW 生命周期回归执行生产 VMO，覆盖重复快照/退出、有存活兄弟、延迟映射退休、偏移与截断窗口。
宿主适配器只替代物理页、调度等待与 MMU；真实页表、TLB 与承载线程退出交错仍由双架构 runtime gate 裁决。

## 页表、共享成员与退休

- 锁序为 VMO 树 → 地址空间；共享地址空间 → 成员空间。缺页不得持地址空间锁等待树锁。
- Mapping 发布后不可变。部分 unmap/protect 准备 replacement 后在空间锁内替换，live 在同锁清除/复查；
  迟到的缺页不能重新安装已退休映射。protect 不得超过创建时的 rights ceiling。
- 页框/页表页的退休顺序固定为：撤销 PTE → local/remote translation fence 完成 → 释放 owner。
  fence 失败必须保活 owner 并 fail-stop；不能用错误展开提前释放。
- 共享区撤销必须传播到全部已登记成员。成员的私有区不跨空间共享页表；硬件身份不作为 logical CPU 索引。
- 地址空间析构先全局 fence，再退休 ASID，再释放页表与映射。ASID 换代和 activation 的握手由 arch owner 保证。
- 设备页表不由 CPU 激活。IOMMU 必须先完成设备侧失效，才消费 DeviceAddressSpace 的 retirement token；
  PMT 随后才能解除 pin。该域的 create/map 只报告页表分配失败，不依赖上层对象的 status codec。

## Pager 与故障信息

- requested map 是未完成请求的唯一 owner；失败删除请求并唤醒，后续访问可重新请求。等待时释放树锁。
- supply 复用源页节点，先保证目标与源窗口合法、源页完整且未固定；目标已存在的页保持不变。
- dirty/writeback 与写保护遵循同一树/映射契约，回写开始后的新写入重新置脏。
- FaultSource 是单次解析结果：是否等待过 pager，不能作为持久 residency cache。只有成功安装翻译并完成 fence
  的 restricted fault 才进入线程统计。normal mode 服务缺页和失败解析不混入该计数。
- restricted fault/pager-wait 计数与既有线程状态同锁更新，无新锁、分配或页级影子索引；native runtime
  只投影该 owner。linuxd 在线程退休时累加，投影 minor/major，删除旧异常层计数，避免重复记账。

## 验证与成本

COW 回收只转移已有页/AVL 节点；祖先遍历位于快照慢路径，普通解析只检查直接父节点。生产 host fixture
提供保留 272 个兄弟的快照/退休 CPU 诊断，不设宿主纳秒阈值；它不能替代共享 VMAR fence 或 native 调度成本。
缺页记账仅在缺页完成时增加既有短锁中的整数更新，由真实匿名缺页、pager 与资源使用 runtime 检查验证。
