# System syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 142 | `reboot` | Partial | initial-user-namespace CAP_SYS_BOOT、restart/halt/poweroff 与 platform reset；CAD_ON/CAD_OFF 返回 EOPNOTSUPP |
| 160 | `uname` | Complete | immutable build fields 与 calling Thread 的 actual UTS hostname/domainname |
| 161 | `sethostname` | Complete | 标准最大64 bytes、namespace-owning user scope CAP_SYS_ADMIN、原子 name publication |
| 162 | `setdomainname` | Complete | 与 sethostname 同一 UTS owner，支持 raw bytes 与空名字 |
| 168 | `getcpu` | Complete | current logical `CpuId` |
| 179 | `sysinfo` | Partial | uptime、memory、process 与 runnable load scope |
| 258 | `riscv_hwprobe` | Partial | value query、logical CPU mask 与 conservative capability |
| 278 | `getrandom` | Complete | RANDOM/NONBLOCK/INSECURE flags 与 initialized hardware entropy façade |

`reboot` 按 Linux `int magic1/int magic2/unsigned int cmd` 解码低 32 位，接受 syscall register
中的零扩展与符号扩展；`RESTART2` 的用户 pointer 仍保留完整 64 位。
权限检查先于 magic/command 解码：固件 reset 要求 initial user namespace 的 CAP_SYS_BOOT。
UID0 不直接旁路 capability，child user namespace 的 pE 不授权全局设备/固件 reset。
PID namespace reboot 的独立 init termination 未开放。

## 已知缺口

`riscv_hwprobe` 的 WHICH_CPUS mode、完整 kernel accounting、hibernate/kexec 与非 RISC-V capability query backend 尚未开放。

当前没有 Ctrl-Alt-Delete input→reset/PID 1 signal 的消费链，因此 CAD_ON/CAD_OFF 明确返回
`EOPNOTSUPP`，不保存未消费的 policy 或返回成功。该范围与固定 Linux `kernel/reboot.c`
中 CAD policy→`ctrl_alt_del` 的完整行为有差异；namespace PID init termination 与 firmware reset 属于不同 owner，不能让 child 复用 global reset。
