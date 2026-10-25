//! Typed libc time operations; TZif persistence and UI policy belong to calendar.

use std::{ffi::CStr, io, sync::Mutex};

use crate::raw;

// OWNER: serialize libc timezone refresh and copying its borrowed abbreviation.
// Without this guard a concurrent tzset could invalidate tm_zone before it is copied.
static TIMEZONE: Mutex<()> = Mutex::new(());

/// A copied local-time projection with no borrowed libc storage.
pub struct LocalTime {
    /// Seconds east of UTC at the queried instant.
    pub offset: i64,
    /// Owned timezone abbreviation, empty when libc provides none.
    pub abbreviation: String,
}

/// Projects Unix `seconds` through the process's standard libc timezone.
///
/// # Returns
/// An owned UTC offset and abbreviation for that instant.
///
/// # Errors
/// Returns the libc error when the timestamp cannot be represented.
pub fn local_time(seconds: i64) -> io::Result<LocalTime> {
    let _guard = TIMEZONE.lock().unwrap_or_else(|error| error.into_inner());
    let result = broken_down(seconds)?;
    let abbreviation = if result.zone.is_null() {
        String::new()
    } else {
        // SAFETY: libc owns the NUL-terminated name; TIMEZONE excludes our refresh until copied.
        unsafe { CStr::from_ptr(result.zone) }
            .to_string_lossy()
            .into_owned()
    };
    Ok(LocalTime {
        offset: result.gmtoff,
        abbreviation,
    })
}

/// Returns seconds east of UTC at Unix `seconds`, without copying an abbreviation.
///
/// # Errors
/// Returns the libc error when the timestamp cannot be represented.
pub fn utc_offset(seconds: i64) -> io::Result<i64> {
    let _guard = TIMEZONE.lock().unwrap_or_else(|error| error.into_inner());
    Ok(broken_down(seconds)?.gmtoff)
}

fn broken_down(seconds: i64) -> io::Result<raw::Tm> {
    let mut result = std::mem::MaybeUninit::<raw::Tm>::zeroed();
    // SAFETY: both pointers are aligned and valid; localtime_r initializes result on success.
    if unsafe { raw::localtime_r(&seconds, result.as_mut_ptr()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the non-null return confirms initialization; callers keep TIMEZONE while reading zone.
    Ok(unsafe { result.assume_init() })
}

/// Asks libc to refresh timezone state after the caller's process-level configuration change.
///
/// # Returns
/// No value or error; serializes with [`local_time`] and never mutates the environment itself.
pub fn refresh_timezone() {
    let _guard = TIMEZONE.lock().unwrap_or_else(|error| error.into_inner());
    // SAFETY: tzset has no pointer parameters and is serialized with our libc timezone readers.
    unsafe { raw::tzset() };
}

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
