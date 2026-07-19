# 启动与平台契约

## Owner

- `bootloader` 独占 M-mode firmware state、machine trap stack、PMP 与 RustSBI service。
- `bootloader::rfence` 独占每 hart 的 request/range/ack mailbox 与 broadcast serialization lock；`STARTS`/`SIZES` 只由持锁 sender 在 request Release 前写，并由目标 hart 在 request Acquire 后读。
- `platform::qemu_virt` 独占 DTB machine facts、firmware operation codec、PLIC context 与具体设备装配。
- `arch::riscv64::startup` 独占 secondary entry 前的 stack 和 hardware/logical projection；`cpu::CpuTopology` 独占进入 generic kernel 后的 identity mapping 与 lifecycle。

## Interface

- bootloader 与 kernel 只通过 entry ABI、DTB opaque 和 SBI 交互；不得共享 Rust state。
- platform 向上只公开 typed `BootInfo`、`PlatformInfo`、firmware operation、interrupt controller 与通用 device façade。
- hardware address、hart ID、SBI status、PLIC context 和 concrete VirtIO adapter 不得进入 generic domain。
- `platform::qemu_virt` 的 PLIC register codec 只编码 source ID `1..=1023`；`0` 是 claim 哨兵，越过单个 context `0x80` enable bitmap 的 ID 必须在 MMIO 前拒绝。
- 新 machine 必须作为独立 compile-time platform backend 接入；禁止在 generic code 追加 target 分支。
- SBI RFENCE 必须保留 `[start_addr, start_addr + size)` 语义并逐页执行 range `SFENCE.VMA`；仅 `(start,size) == (0,0)` 或 `size == usize::MAX` 表示 whole address space，非零 start 的零 size 是空操作。

## Failure and cleanup

- boot capability、DTB、CPU mapping 或 required device 初始化失败时 fail-stop，禁止以默认 topology 或 guessed address 继续。
- PLIC handler 返回错误或 source 未注册时，platform 仍须对每个已 claim vector 恰好 complete 一次。
- secondary publication 使用 Release/Acquire；未完成全局 publication 的 CPU 不得观察或修改 generic state。
- request Release/range Relaxed publication 与目标 request Acquire 配对；ack Release 与 sender Acquire 配对。缺失 range mailbox 会把每次 page revoke 退化为 whole-address-space flush；缺失任一配对会使 sender 在 fence 未完成时释放 translation owner。
