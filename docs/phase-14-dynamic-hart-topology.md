# Phase 14：DTB 驱动的动态 hart topology

日期：2026-07-11（Asia/Shanghai）

## 结果

固定 `MAX_CORES = 8` 已从 runtime topology 中删除：

- bootloader 只让 cold-boot hart 首先进入 kernel，其他 DTB hart 保持 HSM `STOPPED`；
- kernel `BoardInfo` 保存 `hart_mask`、`hart_count` 与 `max_hart_id`，并在 allocator 前验证 cold-boot hart；
- allocator 后构造紧凑的动态 `HartTopology`，每个 DTB hart 拥有 startup stack、processor、softirq pending、online/active 状态；
- boot hart 以相同 `_start` 和 DTB opaque 调用 SBI HSM，secondary 从动态 table 取得自己的 `sp` 后进入 `kmain_secondary`；
- scheduler、softirq、RFENCE target 与 PLIC threshold/enable/affinity 均只消费 DTB hart 集合；
- block IRQ 绑定实际 cold-boot hart，不再写死 CPU0。

bootloader 的 M-mode slot 按 `usize::BITS` 覆盖单字 SBI hart mask 的表达范围；这不是运行时核数。kernel 只为 DTB mask 中真实存在的 hart 分配状态，最终容量受可用内存和 SBI/PLIC/CLINT 表达能力约束。

## 入口静态检查

最终 `_start` 反汇编显示：

1. 动态表地址为未发布哨兵时加载唯一 `boot_stack_top`；
2. secondary 以 0x180-byte `HartState` stride 查找 `a0`，从 entry `+0x8` 加载 `sp`；
3. 两条路径初始化 `gp/tp/sscratch`，分别调用 `kmain_boot` 与 `kmain_secondary`；
4. 找不到 hart 时关闭 S-mode 中断并永久 WFI。

bootloader 最终映像 text+data+bss 为 1,086,996 bytes，仍位于 2 MiB firmware 区间内。

## 非测试 QEMU 验证

仓库规则禁止维护、修正或执行测试用例。本阶段使用相同 bootloader/kernel/fs 镜像完成：

| QEMU 配置 | Boot HART | DTB count/mask | online 结果 | userspace |
|---|---:|---|---|---|
| `-smp 1` | 0 | `1 / 0x1` | `count=1, mask=0x1` | `LiteOS init` |
| `-smp 2` | 1 | `2 / 0x3` | `count=2, mask=0x3` | `LiteOS init` |
| `-smp 8` | 5 | `8 / 0xff` | `count=8, mask=0xff` | `LiteOS init` |

上述结果证明三种已观察配置都由 DTB 集合驱动并完成 HSM secondary 上线；它不证明 OOM、稀疏自定义 DTB、真实硬件 PLIC/CLINT 或超过单字 SBI mask 的拓扑。
