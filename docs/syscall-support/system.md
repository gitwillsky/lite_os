# System syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 142 | `reboot` | Partial | privileged restart/poweroff 与 platform reset |
| 160 | `uname` | Complete | fixed Linux-compatible identity projection |
| 168 | `getcpu` | Complete | current logical `CpuId` |
| 179 | `sysinfo` | Partial | uptime、memory、process 与 runnable load scope |
| 258 | `riscv_hwprobe` | Partial | value query、logical CPU mask 与 conservative capability |
| 278 | `getrandom` | Complete | RANDOM/NONBLOCK/INSECURE flags 与 initialized hardware entropy façade |

## 已知缺口

`riscv_hwprobe` 的 WHICH_CPUS mode、完整 kernel accounting、hibernate/kexec 与非 RISC-V capability query backend 尚未开放。
