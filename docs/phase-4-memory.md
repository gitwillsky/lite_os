# LiteOS Phase 4：物理内存、虚拟内存与用户访问边界

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `02ad425`（Phase 0–3）
> 规范基线：[standards-baseline.md](standards-baseline.md) 中固定的 Rust、RISC-V ISA/Privileged Architecture、ELF 与 psABI 一手资料。
> 验证约束：不维护、不修正、不执行测试；只使用构建、源码/反汇编检查和非测试 QEMU 启动观察。

## 1. 阶段范围

本阶段收口物理页所有权、Sv39 页表、用户/内核地址空间、ELF 装载、用户栈、`brk`、地址空间复制、TLB 同步和 syscall copyin/copyout。当前没有标准 `mmap/mprotect/munmap/fork/clone` syscall，阶段目标不是增加 ABI，而是让现有 `execve/brk` 与内部地址空间复制具备可证明的失败原子性和权限边界。

## 2. 当前实现

- `PhysicalPageNumber`/`PhysicalAddress` 的安全方法可任意制造 `&'static mut`；页表、trap context、信号帧和 syscall 用户缓冲区均依赖该接口。
- `PageTable::find_pte(&self)` 返回 `&mut PageTableEntry`；只读查询与并发写没有类型边界。`PageTable::from_token` 构造不拥有页表帧的伪所有者。
- `translated_byte_buffer` 返回跨页 `Vec<&'static mut [u8]>`，不检查 `U/R/W`、长度溢出或 canonical user range，缺页时 panic。
- `MemorySet` 拥有页表与 `MapArea`；`MapArea` 拥有 framed mapping 的 `FrameTracker`。但 map/append 失败可能留下半事务，shrink 使用 `mem::forget` 泄漏帧。
- ELF loader 接受 W+X segment、未对 program-header 算术做 checked bounds，非页对齐 segment 的文件内容被复制到页首，初始栈只按 8-byte 对齐。
- 内置 dynamic linker、通用 DMA 映射接口和 kernel-stack 泛型 push 没有调用者，扩大了本阶段不安全面。

## 3. 关键调用链

1. syscall → `current_user_token` → `PageTable::from_token` → `translated_byte_buffer` → 可逃逸可变页切片。
2. trap/signal → `Memory::trap_context()` → 地址空间锁释放 → 返回 `&'static mut TrapContext` → syscall/exec 可替换底层映射。
3. ELF → `MapArea::map` → `copy_data` → 页表翻译 → 物理页可变切片；非页对齐 `p_vaddr` 丢失页内偏移。
4. `brk` → `append_to/shrink_to` → PTE 修改与 frame map 修改 → 当前没有完整 rollback/统一远端 TLB 同步。
5. clone helper → 为每个 area 重建映射 → 同时取得源、目的 `&'static mut [u8]` → 深拷贝页内容。

## 4. 关键数据结构

- `FrameTracker`：单个或连续物理页帧的 RAII 所有者；Drop 将 PPN 放回 frame allocator recycler。
- `PageTable`：拥有 root/intermediate 页表帧；leaf data frame 由 `MapArea` 或 kernel identity mapping 的外部生命周期保证。
- `MapArea`：连续 VPN 区间、映射类型、权限和 framed page 的 `BTreeMap<VPN, FrameTracker>`。
- `MemorySet`：地址空间事务边界，拥有 `PageTable + Vec<MapArea>`；task 通过单一 `Mutex<MemorySet>` 共享。
- `TrapContext`：位于 supervisor-only 用户地址空间页中的 72-word 保存区；只能在持有对应 `MemorySet` 锁且映射存活时访问。

## 5. 当前不变量及证据

- Sv39 user canonical range 是 `[0, 2^38)`；trampoline/trap context 位于 sign-extended high half，用户 PTE 不得覆盖。
- RISC-V leaf PTE 的 `W=1,R=0` 是保留编码；新映射必须带 `V/A`，可写页必须带 `D`。用户 copy 还必须检查 `U` 和方向对应的 `R/W`。
- kernel text 为 RX、rodata 为 R、data/bss/stack/MMIO/physmap 为 RW-NX；用户 ELF segment 不允许同时 W+X。
- `FrameTracker` 存活期间其物理页不得回收；引用生命周期不得超过 tracker 或 address-space lock。
- PTE 修改后，本 hart 必须 `SFENCE.VMA`；可能在其他 hart 使用的地址空间必须通过同步 SBI RFENCE 完成 shootdown。

## 6. 已确认问题

| 严重度 | 问题 | 直接后果 |
|---|---|---|
| Blocker | 用户 copy 与 trap context 返回 `&'static mut` | 映射替换/回收后 use-after-free，或多个可变引用造成 Rust UB |
| Blocker | 用户 copy 不检查权限、overflow、跨页 fault | kernel panic；可读写 supervisor-only 页；无法返回 Linux `EFAULT` |
| Critical | `find_pte(&self) -> &mut PTE` | shared reference 可写页表，破坏 aliasing 与锁证明 |
| Critical | shrink `mem::forget(FrameTracker)` | 每次堆收缩永久泄漏物理帧 |
| Critical | map/append 没有完整 rollback | PTE、area range 与 frame ownership 不一致 |
| Critical | ELF 非对齐 LOAD 复制到页首、算术未检查 | 程序映像错误或越界 slice panic |
| Critical | ELF 允许 W+X 和动态 ELF 走未完成静态路径 | 违反 W^X；执行语义不可证明 |
| Major | 初始栈 8-byte 对齐且写 helper 静默跳过缺页 | 违反 riscv64 psABI 16-byte 对齐；exec 成功但栈内容残缺 |
| Major | `recycle_data_pages` 先 drop frame 后保留旧 PTE | exec 提交窗口出现指向已回收页的映射 |
| Major | 孤儿 dynamic-linker/DMA/kernel-stack push API | 大量无调用 unsafe 与未完成语义持续进入审计面 |

## 7. 对应标准

- Rust Reference/Rustonomicon：从 raw pointer 构造引用必须证明有效、对齐、生命周期和 aliasing；锁保护的数据引用不能逃逸 guard。
- RISC-V Privileged Architecture：Sv39 canonical address、leaf/non-leaf PTE 编码、`U/R/W/X/A/D/G` 权限和 `SFENCE.VMA` 次序。
- RISC-V psABI：过程入口 stack pointer 必须 16-byte 对齐；ELF64 little-endian RISC-V machine/header 与 LOAD segment 约束。
- Linux syscall 语义：用户地址 fault 返回 `EFAULT`；零长度 I/O 不解引用用户指针；部分 I/O 只能报告已经完成的字节数。

## 8. 目标模型

- 物理地址类型只暴露 raw pointer；所有 `unsafe` 解引用收缩到有明确 owner/lock 证明的页表、frame 和 `MemorySet` 方法中。
- 页表只读 walk 返回 PTE 值；写 walk 要求 `&mut PageTable`。禁止从 token 构造伪 `PageTable` 所有者。
- `MemorySet::copy_from_user/copy_to_user` 在地址空间锁内逐页复制，不分配页片段集合，不返回用户引用，完整检查 range、leaf、`U` 与 `R/W`。
- `Memory` 提供 copy/string/trap-context 值语义 façade；syscall 和 signal 不直接接触 token/物理页。
- map/append/ELF/exec 使用 prepare → validate → commit；失败时 PTE、frame map、area range保持原状态。
- 只保留当前静态 ELF eager paging；删除未接通 dynamic linker、generic DMA mapping façade 和无调用 unsafe helper。

## 9. 删除项

- `translated_byte_buffer`、`translated_str`、`translated_ref_mut`、`PageTable::from_token`。
- `PhysicalAddress::get_mut`、`PhysicalPageNumber::{get_bytes_array_mut,get_pte_array,get_mut}` 安全静态引用接口。
- 无调用的内置 dynamic linker 模块及相关 `MemorySet` 状态/API。
- 无调用的 `MemorySet` generic DMA allocation/mapping API 与 `KernelStack::push_on_top`。
- exec 前的 `recycle_data_pages` 双阶段替换路径。

## 10. 修改计划

1. 重写物理页和页表访问边界 → verify: safe API 不再返回 `'static mut`，read walk 不可写。
2. 实现锁内 copyin/copyout/string/word 与 trap-context 值访问 → verify: syscall/signal 无 token translation，跨页与权限错误返回 `EFAULT`。
3. 修复 `MapArea` rollback/shrink/frame ownership 和 TLB transaction → verify: 每个 PTE 与 tracker 一一对应，失败不改变 area range。
4. 收紧 ELF/stack/exec → verify: checked bounds、页内偏移、W^X、16-byte stack、单锁替换。
5. 删除孤儿内存子系统与多余 allocator layer（若构建/启动证明无需）→ verify: 删除项零调用且不改变公开 Linux ABI。
6. 执行构建、源码/ELF/反汇编检查和 8-hart QEMU 观察 → verify: init 创建且无 panic；不运行测试。

## 11. 验证方式

- `git diff --check`、`cargo check --workspace`、`make build-kernel`、`make build-bootloader`、`make build-user`。
- 搜索 `&'static mut`、`from_token`、`translated_*`、物理地址直接解引用、PTE flag 组合和地址算术。
- 检查 kernel/user ELF program headers、section 权限与用户入口/栈对齐。
- 心智验收零长度、首/尾页 fault、跨页 struct/string、地址 overflow、supervisor page、R-only/W-only、OOM rollback、non-page-aligned LOAD、exec 替换。
- QEMU `virt -smp 8` 非测试启动观察；遵守仓库规则，不运行、维护或修正测试。

## 12. 风险与阶段边界

- 当前没有 ASID 分配；所有地址空间切换仍使用 ASID 0，因此保留全量 local/remote fence，性能优化不先于正确性。
- 当前只支持 eager paging；lazy fault/COW 在没有标准 fork/mmap ABI 和统一 VM object 前不引入。
- `brk` 的 Linux 语义在现有 syscall 范围内完成；标准 mmap/mprotect/munmap 留待其 ABI 被明确加入后实现。
- DMA ownership、MMIO cacheability 和 VirtIO barrier 属于 Phase 10；本阶段只删除 `MemorySet` 中无调用的通用 façade，不改变驱动现有 DMA 路径。
- 当前 recycler 使用空闲页页首保存 intrusive next PPN，不再受 128 MiB 固定数组限制；它仍采用线性重复释放检查，未来只有在实际 profile 证明需要时才引入 bitmap。

## 13. 完成结论

Phase 4 已于 2026-07-11 完成。当前用户地址访问只允许在 `MemorySet` 锁内完成值复制，页表只读 walk 不再产生可变引用，物理页类型不再提供安全的 `'static mut` 接口。现有 Linux ABI 未增加私有 syscall；`brk/execve/read/write/nanosleep/signal` 已迁移到统一地址空间边界。

### 13.1 实际内存与页表模型

- `FrameTracker` 是物理页唯一 RAII owner。回收链表使用已经释放页的首个 machine word 保存 next PPN，不分配、不限制 DTB memory size；重复/越界释放是 kernel invariant failure，不再静默忽略。
- kernel global allocator 从 hybrid buddy+custom slab 收敛为一个 4 MiB IRQ-safe buddy allocator，删除 558 行 slab unsafe；实际 8-hart 启动证明当前最小系统不需扩大静态 heap。
- `PageTable` 只拥有 root/intermediate frame；read walk 返回 `PageTableEntry` 值，write walk 要求 `&mut PageTable` 并使用有证明的 scoped volatile raw access。root/intermediate OOM 可返回，不再由用户 ELF 触发 panic。
- leaf mapping 统一补 `V/A`，可写页补 `D`，拒绝 `W=1,R=0` 与 W+X；translate 同样拒绝保留 leaf 编码。trampoline 是 supervisor RX global mapping，trap context 是 supervisor RW-NX mapping。
- `MapArea` 在 leaf map 成功后才提交 `FrameTracker`；append 失败回滚本次所有页，shrink 正常 Drop frame，删除原 `mem::forget` 泄漏。中间页表页只在整个 `PageTable` Drop 时回收。

### 13.2 用户访问与地址空间事务

- 删除 `PageTable::from_token` 和全部 `translated_*`。`copy_from_user/copy_to_user` 先完整验证 canonical low-half range、overflow、leaf、`U` 与方向权限，再逐页复制；fault 不向 kernel/user 目标留下前缀修改。
- `Memory` façade 在持有 `Mutex<MemorySet>` 时完成 copy/string/execute/write validation。`execve` 的跨页 pointer array、`nanosleep` 的跨页 timespec、signal frame 与 syscall I/O 不再直接翻译 token。
- trap context 只按 owned `TrapContext` clone 读写；底层物理引用不越过地址空间 guard。exec 使用单次 `MemorySet` 赋值提交，不再先清 frame、后保留 stale PTE。
- `read/write` 使用 4 KiB kernel-stack chunk，避免用户长度直接触发大额 heap allocation；中途 user fault 或文件短 I/O 返回已经完成的字节数，零长度不解引用指针。
- signal handler/return PC 必须是 `U|X` mapping；signal frame 显式编码以避免读取 padding，恢复 `sstatus` 时强制 `SPP=User`、`SIE=0`、`SPIE=1`、`SUM=0`、`MXR=0`，阻止伪造 frame 返回 S-mode。

### 13.3 ELF、栈与 program break

- loader 只接受 ELF64 little-endian RISC-V executable，拒绝 `PT_INTERP/PT_DYNAMIC`、越界 program header 数据、`filesz > memsz`、非法 alignment、W+X 与非 `U|X` entry。
- 非页对齐 LOAD 的文件字节从正确页内偏移复制，剩余 BSS 由 zeroed frame 提供；所有地址、文件范围、栈和参数算术使用 checked operations。
- 用户栈位于 Sv39 low half 顶部，上下各有 unmapped guard；初始 SP 按 riscv64 psABI 16-byte 对齐。heap base 是最高 LOAD 的 page-aligned 末端，limit 是下方 stack guard，二者不再使用固定 1 GiB/2 GiB 地址。
- program break 与 heap `MapArea` 同属 `MemorySet` guard；增长失败完整 rollback，收缩回收 frame，页粒度变化后同步 SBI RFENCE。Linux `brk` 失败返回旧 break，而非负 errno。

### 13.4 删除与收敛

- 删除 871 行未接通 dynamic linker 及其 `MemorySet` 状态/API。
- 删除 558 行 custom slab allocator、hybrid dispatch 和 init 顺序。
- 删除无调用的 `MemorySet` generic DMA mapping façade、address-space deep clone helper、kernel-stack generic push、raw current-user-token helper与 frame arbitrary dealloc/stat façade。
- VirtIO 仅保留返回物理地址值的 kernel translation API；DMA owner/barrier 的领域重构仍归 Phase 10。

### 13.5 验证结果

- `git diff --check` 与定向 nightly rustfmt 成功；源码检索确认 memory/task/signal/syscall/trap 中不存在 `translated_*`、`PageTable::from_token`、物理页 `get_* -> &'static mut`、dynamic linker、custom slab 或 `mem::forget(FrameTracker)`。
- `cargo check --workspace`、`make build-user`、`make build-kernel`、`make build-bootloader` 与 ext2 image 创建全部成功；最终 kernel 告警 311，Phase 3 基线为 330。
- `llvm-objdump` 确认 init 是 `elf64-littleriscv`，唯一 LOAD 为 RX，地址/offset 均 4 KiB 对齐，无动态段。
- QEMU `virt -smp 8` 观察覆盖 cold-boot hart 3/kernel owner hart 0 与最终 cold-boot hart 0/kernel owner hart 2；8 个 hart 全部启动，单一 4 MiB buddy allocator 完成 PLIC、VirtIO block、ext2 与 signal 初始化并创建 init。观察窗口内无 OOM、panic、page fault 或异常输出后正常终止。
- 遵守仓库规则，未执行、维护或修正任何测试。

### 13.6 阶段边界

- 当前 syscall ABI 没有标准 mmap/mprotect/munmap/fork/clone，因此没有引入 VM object、lazy fault 或 COW；这些能力必须随明确 ABI/进程模型落地，不能先保留无调用 façade。
- 当前没有 ASID allocator，地址空间切换继续使用 ASID 0 并全量 local fence；远端页表更新使用同步 SBI RFENCE。ASID 性能优化不能改变本阶段 shootdown 正确性。
- ELF auxv、TLS、musl 动态装载和完整 exec credential 语义属于 Phase 12；本阶段明确拒绝动态 ELF，而不是让未完成 linker 猜测执行。
- DMA ownership、IOMMU/cacheability 与 VirtIO ring barrier 归 Phase 10；filesystem page cache/writeback 与 O_DIRECT/DMA user-page pinning 分别归 Phase 8/10。
