//! kernel 主动产生或按编号判定语义的 Linux asm-generic signal number 与 `si_code`。
//!
//! 只收录 kernel 自身使用的编号；userspace 传入的任意合法编号仍以 `usize` 原样处理。

pub const SIGHUP: usize = 1;
pub const SIGINT: usize = 2;
pub const SIGQUIT: usize = 3;
pub const SIGILL: usize = 4;
pub const SIGTRAP: usize = 5;
pub const SIGBUS: usize = 7;
pub const SIGKILL: usize = 9;
pub const SIGSEGV: usize = 11;
pub const SIGPIPE: usize = 13;
pub const SIGALRM: usize = 14;
pub const SIGCHLD: usize = 17;
pub const SIGCONT: usize = 18;
pub const SIGSTOP: usize = 19;
pub const SIGTSTP: usize = 20;
pub const SIGTTIN: usize = 21;
pub const SIGTTOU: usize = 22;
pub const SIGURG: usize = 23;
pub const SIGXCPU: usize = 24;
pub const SIGXFSZ: usize = 25;
pub const SIGWINCH: usize = 28;

/// `SIGILL`：非法 opcode。
pub const ILL_ILLOPC: i32 = 1;
/// `SIGILL`：非法 trap。
pub const ILL_ILLTRP: i32 = 4;
/// `SIGTRAP`：breakpoint。
pub const TRAP_BRKPT: i32 = 1;
/// `SIGBUS`：不存在的物理地址或越过 backing object 末尾。
pub const BUS_ADRERR: i32 = 2;
/// `SIGSEGV`：地址未被任何 mapping 覆盖。
pub const SEGV_MAPERR: i32 = 1;
/// `SIGSEGV`：mapping 存在但不允许该访问。
pub const SEGV_ACCERR: i32 = 2;
