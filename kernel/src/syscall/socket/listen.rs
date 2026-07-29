/// Linux-wide maximum applied before a protocol backend receives `listen(2)`.
pub(crate) const BACKLOG_MAX: usize = 4096;

/// Normalizes the raw `listen(2)` backlog according to the Linux signed-32 ABI.
///
/// # Parameters
///
/// - `backlog`: Raw syscall-register value for the C `int` argument.
///
/// # Returns
///
/// A queue depth in `0..=BACKLOG_MAX`. Negative C values compare as unsigned
/// and therefore select the kernel limit.
///
/// Rust `std::os::unix::net::UnixListener` passes `-1` on Linux because Linux
/// caps the unsigned projection at `somaxconn`. Clamping the signed value to
/// zero first leaves every standard listener with one effective pending slot,
/// so a second concurrent connect incorrectly fails with `EAGAIN`.
pub(crate) fn normalize_backlog(backlog: isize) -> usize {
    (backlog as u32 as usize).min(BACKLOG_MAX)
}
