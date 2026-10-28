//! Runtime loading of an upstream shared object (`dlopen`/`dlsym`).

#[cfg(target_os = "linux")]
use crate::raw;
#[cfg(target_os = "linux")]
use std::ffi::CStr;
use std::{
    ffi::{CString, c_void},
    io,
    ptr::NonNull,
};

/// An open shared object; the handle is closed on drop.
///
/// Symbols obtained from it must not be used after it is dropped. Callers that
/// keep function pointers store the `Library` next to them.
pub struct Library {
    // Only Linux can construct a `Library`; other targets fail in `open`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    handle: NonNull<c_void>,
}

// SAFETY: a `dlopen` handle is a process-wide reference count on the object, not thread-affine state.
unsafe impl Send for Library {}
// SAFETY: `dlsym` and `dlclose` are thread-safe in musl; the handle is never mutated after creation.
unsafe impl Sync for Library {}

impl Library {
    /// Opens `name` (a soname or absolute path) with lazy binding and global symbols disabled.
    ///
    /// # Errors
    /// `InvalidInput` for an embedded NUL; otherwise `NotFound`/`Other` carrying the loader's message
    /// (missing file, missing dependency, wrong architecture). Non-Linux targets return `Unsupported`.
    pub fn open(name: &str) -> io::Result<Self> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "library name contains NUL")
        })?;
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `name` is a valid C string for the duration of the call.
            let handle = unsafe { raw::dlopen(name.as_ptr(), raw::RTLD_LAZY | raw::RTLD_LOCAL) };
            NonNull::new(handle)
                .map(|handle| Self { handle })
                .ok_or_else(last_error)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    /// Resolves `name` as a function pointer of type `T`.
    ///
    /// # Safety
    /// `T` must be a pointer-sized `extern "C"` function pointer type whose signature matches the
    /// upstream symbol, and the pointer must not outlive `self`.
    ///
    /// # Errors
    /// `NotFound` when the object does not export the symbol.
    pub unsafe fn symbol<T: Copy>(&self, name: &str) -> io::Result<T> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "symbol name contains NUL"))?;
        #[cfg(target_os = "linux")]
        {
            // SAFETY: handle is live; `name` is a valid C string.
            let address = unsafe { raw::dlsym(self.handle.as_ptr(), name.as_ptr()) };
            if address.is_null() {
                return Err(last_error());
            }
            // SAFETY: the caller guarantees `T` is a function pointer type; sizes were checked above.
            Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&address) })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        // SAFETY: the handle came from a successful dlopen and is closed exactly once.
        unsafe {
            raw::dlclose(self.handle.as_ptr());
        }
    }
}

#[cfg(target_os = "linux")]
fn last_error() -> io::Error {
    // SAFETY: dlerror returns null or a NUL-terminated message valid until the next dl call on this thread.
    let message = unsafe { raw::dlerror() };
    let text = if message.is_null() {
        "dynamic loader failed".to_owned()
    } else {
        // SAFETY: non-null per the check above.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    io::Error::new(io::ErrorKind::NotFound, text)
}
