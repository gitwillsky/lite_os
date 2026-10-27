//! Linux ptrace request 与当前 regset selector；值取自固定 Linux UAPI。
/// 由真实 parent 跟踪 calling Thread。
pub const PTRACE_TRACEME: usize = 0;
/// 从 tracee 的 instruction mapping 读取一个 native word。
pub const PTRACE_PEEKTEXT: usize = 1;
/// 从 tracee 的 data mapping 读取一个 native word。
pub const PTRACE_PEEKDATA: usize = 2;
/// 向 tracee 的 instruction mapping 写入一个 native word。
pub const PTRACE_POKETEXT: usize = 4;
/// 向 tracee 的 data mapping 写入一个 native word。
pub const PTRACE_POKEDATA: usize = 5;
/// 恢复 tracee，data 指定待交付信号。
pub const PTRACE_CONT: usize = 7;
/// 对已附加目标生成 SIGKILL。
pub const PTRACE_KILL: usize = 8;
/// 附加指定 Thread 并生成 SIGSTOP。
pub const PTRACE_ATTACH: usize = 16;
/// 解除附加并恢复指定信号。
pub const PTRACE_DETACH: usize = 17;
/// 取得 signal-delivery stop 的 siginfo。
pub const PTRACE_GETSIGINFO: usize = 0x4202;
/// 替换 signal-delivery stop 的 siginfo。
pub const PTRACE_SETSIGINFO: usize = 0x4203;
/// 按 ELF note selector 和 iovec 读取寄存器。
pub const PTRACE_GETREGSET: usize = 0x4204;
/// 按 ELF note selector 和 iovec 写入寄存器。
pub const PTRACE_SETREGSET: usize = 0x4205;
/// Linux ELF general-purpose register note，用于 GET/SETREGSET。
pub const NT_PRSTATUS: usize = 1;
