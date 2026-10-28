# smb-dtyp 0.12.1

来源：https://crates.io/crates/smb-dtyp/0.12.1 ，原摘要见 UPSTREAM.sha256.json。
FileTime::date_time 复用 since_epoch 的秒/纳秒商余构造，修复当前日期乘 100 溢出。移除上游独立测试与构建依赖；保留源码内风险边界单测。
