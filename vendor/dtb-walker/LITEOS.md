# LiteOS DTB walker dependency

唯一编译来源是本目录，源自 crates.io dtb-walker 0.2.0-alpha.3（MIT）。原文件摘要见
`UPSTREAM.sha256.json`，上游未归一化 manifest 保留于 `Cargo.toml.orig`，本地 Cargo.toml 使用该原始 manifest；
未纳入无生产引用的示例与文档资源。

`src/walker.rs`：消费 `#address-cells`、`#size-cells` 与 `#interrupt-cells` 更新内部上下文后，
也向既有 callback 输出原始 `Property::General`。原版本静默吞掉这三个属性，控制器 binding
无法核对自己的 cell 数。Reg 解码、遍历次序与 callback 的控制语义保持一致；没有第二套 DTB parser。

不引入堆分配、新状态或公共类型。新增调用只出现在启动冷解析，不增设时延 benchmark；
现有实际 QEMU DTB decoder 用例验证属性可见性及生产装配，不模拟硬件中断。

`src/context.rs`：显式标出 `name()` 返回的借用 lifetime，满足当前 Rust 编译器的
`mismatched_lifetime_syntaxes`，保持原借用语义，不放宽上游 warning 策略。
