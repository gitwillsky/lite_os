//! kernel 主动产生或按编号判定语义的 Linux asm-generic signal number 与 `si_code`。
//!
//! 只收录 kernel 自身使用的编号；userspace 传入的任意合法编号仍以 `usize` 原样处理。

pub(crate) const SIGHUP: usize = 1;
pub(crate) const SIGILL: usize = 4;
pub(crate) const SIGTRAP: usize = 5;
pub(crate) const SIGBUS: usize = 7;
pub(crate) const SIGKILL: usize = 9;
pub(crate) const SIGSEGV: usize = 11;
pub(crate) const SIGPIPE: usize = 13;
pub(crate) const SIGALRM: usize = 14;
pub(crate) const SIGCHLD: usize = 17;
pub(crate) const SIGCONT: usize = 18;
pub(crate) const SIGSTOP: usize = 19;
pub(crate) const SIGTTIN: usize = 21;
pub(crate) const SIGTTOU: usize = 22;
pub(crate) const SIGURG: usize = 23;
pub(crate) const SIGXCPU: usize = 24;
pub(crate) const SIGXFSZ: usize = 25;
pub(crate) const SIGWINCH: usize = 28;

/// `SIGILL`：非法 opcode。
pub(crate) const ILL_ILLOPC: i32 = 1;
/// `SIGILL`：非法 trap。
pub(crate) const ILL_ILLTRP: i32 = 4;
/// `SIGTRAP`：breakpoint。
pub(crate) const TRAP_BRKPT: i32 = 1;
/// `SIGBUS`：不存在的物理地址或越过 backing object 末尾。
pub(crate) const BUS_ADRERR: i32 = 2;
/// `SIGSEGV`：地址未被任何 mapping 覆盖。
pub(crate) const SEGV_MAPERR: i32 = 1;
/// `SIGSEGV`：mapping 存在但不允许该访问。
pub(crate) const SEGV_ACCERR: i32 = 2;
