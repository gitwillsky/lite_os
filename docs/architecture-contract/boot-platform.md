# 启动与平台契约

## Owner

- RISC-V `bootloader` 独占 M-mode firmware state、machine trap stack、PMP 与 RustSBI service；AArch64 不得依赖该 domain。
- `bootloader::rfence` 独占每 hart 的 request/range/ack mailbox 与 broadcast serialization lock；`STARTS`/`SIZES` 只由持锁 sender 在 request Release 前写，并由目标 hart 在 request Acquire 后读。
- `platform::qemu_virt::riscv64` 独占 SBI/PLIC machine codec；`platform::qemu_virt::aarch64` 独占 PSCI/GICv3/PL011/PL031 machine codec。共同 façade 独占 DTB machine facts 与具体设备装配。
- `arch::<target>::io` 独占 MMIO 指令与 normal-memory/device ordering mechanism；通用
  `drivers::hal::MmioBus` 只做 window 边界/对齐验证并通过静态 façade 访问，具体 adapter
  不得直接选择 target 指令形态。
- 各 `arch::<target>::startup` 独占 secondary entry 前的 stack 和 raw identity projection；`cpu::CpuTopology` 独占进入 generic kernel 后的 identity mapping 与 lifecycle。

## Interface

- RISC-V bootloader 与 kernel 只通过 entry ABI、DTB opaque 和 SBI 交互；AArch64 只通过 Linux Image header、x0 DTB 与 PSCI HVC 交互。两者都不得共享 Rust state。
- platform 向上只公开 typed `BootInfo`、`PlatformInfo`、firmware operation、linear interrupt token 与通用 device façade。GIC/PLIC claim backend 必须直接构造 `Timer/Device/Software/Spurious`
  语义 variant；禁止用 `u8 kind` 私有 ABI 再做第二次运行时翻译。
- hardware address、hart ID、SBI status、PLIC context 和 concrete VirtIO adapter 不得进入 generic domain。
- `platform::qemu_virt` 的 PLIC register codec 只编码 source ID `1..=1023`；`0` 是 claim 哨兵，越过单个 context `0x80` enable bitmap 的 ID 必须在 MMIO 前拒绝。
- 新 machine 必须作为独立 compile-time platform backend 接入；禁止在 generic code 追加 target 分支。
- AArch64 backend 必须验证 GICv3、PSCI HVC、PL011、PL031 与 `dma-coherent`；不支持的 GICv2/ITS/MSI/PCI、SMC、ACPI 或 guessed MMIO address 必须拒绝。
- AArch64 byte/word/doubleword MMIO 必须分别固定为 exact base-register
  `LDRB/STRB/LDR/STR`；禁止 LLVM 生成 post-index、unscaled、pair 或 SIMD device access。
  缺失此 seam 时 VirtIO input 的连续 config byte 读取会合并成 writeback `LDRB`，QEMU HVF
  无有效 access syndrome 并在 host `hvf_handle_exception` 中 fail-stop。
- PL011 必须在清 pending/unmask RX 与 receive-timeout IRQ 前启用 FIFO，并把 RX threshold 设为最低
  architected level；保持 reset character mode 会把接收缓冲缩成 1 byte，在并发 device IRQ 下丢失
  QEMU stdio 批量输入。handler 仍须按固定 hardirq budget drain，禁止在 IRQ 中等待后续字符。
- SBI RFENCE 必须保留 `[start_addr, start_addr + size)` 语义并逐页执行 range `SFENCE.VMA`；仅 `(start,size) == (0,0)` 或 `size == usize::MAX` 表示 whole address space，非零 start 的零 size 是空操作。

## Failure and cleanup

- boot capability、DTB、CPU mapping 或 required device 初始化失败时 fail-stop，禁止以默认 topology 或 guessed address 继续。
- PLIC handler 返回错误或 source 未注册时，platform 仍须对每个已 claim vector 恰好 complete 一次。
- secondary publication 使用 Release/Acquire；未完成全局 publication 的 CPU 不得观察或修改 generic state。
- GICv3 claim 产生的 opaque token 必须在同一 CPU exactly-once EOI；timer PPI 必须在 EOI 前重新 arm，software SGI 必须在 EOI 后消费同步 request。
- request Release/range Relaxed publication 与目标 request Acquire 配对；ack Release 与 sender Acquire 配对。缺失 range mailbox 会把每次 page revoke 退化为 whole-address-space flush；缺失任一配对会使 sender 在 fence 未完成时释放 translation owner。
