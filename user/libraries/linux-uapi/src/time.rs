//! Typed realtime clock operations; TZif parsing and reload belong to calendar.
use crate::raw;
use std::io;

/// Sets CLOCK_REALTIME to Unix `seconds`, with zero nanoseconds.
///
/// # Returns
/// `()` after the kernel applies the new realtime value.
///
/// # Errors
/// Returns the kernel's permission or timestamp error unchanged.
pub fn set_realtime(seconds: i64) -> io::Result<()> {
    let time = raw::Timespec {
        seconds,
        nanoseconds: 0,
    };
    // SAFETY: CLOCK_REALTIME is zero on the supported libc targets; time is initialized and borrowed.
    if unsafe { raw::clock_settime(0, &time) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
