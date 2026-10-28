//! Client for the upstream libmpv: properties, commands, events and the software render API.
//!
//! The shared object is opened at run time ([`linux_uapi::dynamic_library`]); nothing links against it.
//! mpv calls the wakeup and render-update callbacks from its own threads. They only write one byte to a
//! non-blocking socket, which the caller waits on and then drains with [`Mpv::poll`] and [`Mpv::render_ready`].

extern crate liteos_runtime as _;

mod ffi;

use ffi::{Api, EventRecord, Handle, Param, RenderContext};
use linux_uapi::dynamic_library::Library;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    io::{self, Write},
    os::unix::net::UnixStream,
    ptr,
};

/// Soname installed by `scripts/mpv_runtime.py`.
pub const LIBRARY: &str = "libmpv.so.2";

/// A property value in one of the formats the player reads.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// The property exists but has no value (for example `duration` before a file is loaded).
    None,
    Flag(bool),
    Int(i64),
    Double(f64),
    Text(String),
}

/// The format a property is observed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Flag,
    Int,
    Double,
    Text,
}

impl Format {
    fn code(self) -> i32 {
        match self {
            Self::Text => ffi::FORMAT_STRING,
            Self::Flag => ffi::FORMAT_FLAG,
            Self::Int => ffi::FORMAT_INT64,
            Self::Double => ffi::FORMAT_DOUBLE,
        }
    }
}

/// Why playback of a file ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The file played to its end.
    Finished,
    /// Stopped by a command or by loading another file.
    Stopped,
    /// The core is quitting.
    Quit,
    /// Decoding or demuxing failed; carries mpv's message.
    Failed(String),
    Redirected,
}

/// One event from [`Mpv::poll`].
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    StartFile,
    FileLoaded,
    EndFile(EndReason),
    Seek,
    PlaybackRestart,
    VideoReconfig,
    AudioReconfig,
    /// An observed property changed; `id` is the number given to [`Mpv::observe`].
    Property {
        id: u64,
        name: String,
        value: Value,
    },
    Shutdown,
    /// The event queue overflowed and events were dropped; observed state must be re-read.
    QueueOverflow,
}

/// Writes one byte per mpv callback; the heap address is handed to C, so it never moves.
struct Wake {
    writer: UnixStream,
}

extern "C" fn wake(context: *mut c_void) {
    // SAFETY: `context` is the `Wake` boxed in `Mpv`; both callbacks are unregistered before it is freed.
    let wake = unsafe { &*context.cast::<Wake>() };
    // A full socket already holds a pending wake-up, so WouldBlock needs no retry.
    let _ = (&wake.writer).write(&[1]);
}

/// An initialised mpv core with a software render context.
pub struct Mpv {
    api: Api,
    handle: *mut Handle,
    render: *mut RenderContext,
    wake: Box<Wake>,
    // Dropped last: every pointer above is only valid while the object stays mapped.
    _library: Library,
}

fn check(api: &Api, code: i32) -> io::Result<i32> {
    if code >= 0 {
        return Ok(code);
    }
    Err(io::Error::other(describe(api, code)))
}

fn describe(api: &Api, code: i32) -> String {
    // SAFETY: mpv_error_string returns a static NUL-terminated string for any code.
    let text = unsafe { (api.error_string)(code) };
    if text.is_null() {
        return format!("mpv error {code}");
    }
    // SAFETY: non-null per the check above.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

fn c_string(value: &str) -> io::Result<CString> {
    CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string contains NUL"))
}

impl Mpv {
    /// Loads libmpv, applies `options`, initialises the core and creates the software render context.
    ///
    /// Returns the core and the read end of the wake-up socket: it becomes readable whenever mpv has
    /// events or a new video frame; drain it and then call [`Mpv::poll`] / [`Mpv::render_ready`].
    ///
    /// # Errors
    /// `NotFound` when libmpv or one of its dependencies cannot be loaded, `InvalidInput` for a rejected
    /// option, other kinds for core or render-context creation failures.
    pub fn create(options: &[(&str, &str)]) -> io::Result<(Self, UnixStream)> {
        let library = Library::open(LIBRARY)?;
        // SAFETY: the signatures in `ffi::Api` mirror mpv 0.40's client.h/render.h.
        let api = unsafe { Api::load(&library)? };
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        // SAFETY: mpv_create has no preconditions; a null result means the core could not be allocated.
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err(io::Error::other("mpv_create failed"));
        }
        let mut player = Self {
            api,
            handle,
            render: ptr::null_mut(),
            wake: Box::new(Wake { writer }),
            _library: library,
        };
        for (name, value) in options {
            player.set_option(name, value)?;
        }
        // SAFETY: handle is live and not yet initialised.
        check(&player.api, unsafe { (player.api.initialize)(handle) })?;
        player.create_render_context()?;
        Ok((player, reader))
    }

    fn create_render_context(&mut self) -> io::Result<()> {
        let mut params = [
            Param {
                kind: ffi::PARAM_API_TYPE,
                data: c"sw".as_ptr().cast_mut().cast(),
            },
            Param {
                kind: ffi::PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let mut context = ptr::null_mut();
        // SAFETY: params is a zero-terminated array valid for the call; the handle is initialised.
        check(&self.api, unsafe {
            (self.api.render_create)(&mut context, self.handle, params.as_mut_ptr())
        })?;
        self.render = context;
        let data = ptr::addr_of!(*self.wake).cast_mut().cast::<c_void>();
        // SAFETY: both callbacks only touch `Wake`, which lives until `Drop` unregisters them.
        unsafe {
            (self.api.set_wakeup_callback)(self.handle, Some(wake), data);
            (self.api.render_set_update_callback)(self.render, Some(wake), data);
        }
        Ok(())
    }

    /// Sets an option before initialisation or a property afterwards (`set_property_string` semantics).
    pub fn set_option(&self, name: &str, value: &str) -> io::Result<()> {
        let (name, value) = (c_string(name)?, c_string(value)?);
        // SAFETY: handle is live; both strings are NUL-terminated and borrowed for the call.
        check(&self.api, unsafe {
            (self.api.set_option_string)(self.handle, name.as_ptr(), value.as_ptr())
        })?;
        Ok(())
    }

    /// Sets a property at run time.
    pub fn set_property(&self, name: &str, value: &str) -> io::Result<()> {
        let (name, value) = (c_string(name)?, c_string(value)?);
        // SAFETY: as in `set_option`.
        check(&self.api, unsafe {
            (self.api.set_property_string)(self.handle, name.as_ptr(), value.as_ptr())
        })?;
        Ok(())
    }

    /// Runs a command such as `["loadfile", path, "replace"]`.
    ///
    /// # Errors
    /// The mpv error text, for example for an unknown command or a rejected argument.
    pub fn command(&self, arguments: &[&str]) -> io::Result<()> {
        let owned: Vec<CString> = arguments
            .iter()
            .map(|argument| c_string(argument))
            .collect::<io::Result<_>>()?;
        let mut pointers: Vec<*const c_char> =
            owned.iter().map(|argument| argument.as_ptr()).collect();
        pointers.push(ptr::null());
        // SAFETY: the array is NULL-terminated and every element outlives the call.
        check(&self.api, unsafe {
            (self.api.command)(self.handle, pointers.as_mut_ptr())
        })?;
        Ok(())
    }

    /// Reads a property as text; `None` when it is unavailable.
    pub fn text(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        // SAFETY: handle is live; the returned buffer is copied and released with mpv_free.
        let raw = unsafe { (self.api.get_property_string)(self.handle, name.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: mpv returns a NUL-terminated string that stays valid until mpv_free.
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `raw` came from mpv_get_property_string and is freed exactly once.
        unsafe { (self.api.free)(raw.cast()) };
        Some(text)
    }

    /// Reads a floating-point property; `None` when it is unavailable.
    pub fn double(&self, name: &str) -> Option<f64> {
        let mut value = 0f64;
        self.read(name, ffi::FORMAT_DOUBLE, ptr::addr_of_mut!(value).cast())
            .then_some(value)
    }

    /// Reads an integer property; `None` when it is unavailable.
    pub fn int(&self, name: &str) -> Option<i64> {
        let mut value = 0i64;
        self.read(name, ffi::FORMAT_INT64, ptr::addr_of_mut!(value).cast())
            .then_some(value)
    }

    /// Reads a boolean property; `None` when it is unavailable.
    pub fn flag(&self, name: &str) -> Option<bool> {
        let mut value = 0i32;
        self.read(name, ffi::FORMAT_FLAG, ptr::addr_of_mut!(value).cast())
            .then_some(value != 0)
    }

    fn read(&self, name: &str, format: i32, out: *mut c_void) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: `out` points to a value of the size `format` requires, owned by the caller for this call.
        unsafe { (self.api.get_property)(self.handle, name.as_ptr(), format, out) >= 0 }
    }

    /// Starts delivering [`Event::Property`] with this `id` whenever `name` changes.
    pub fn observe(&self, id: u64, name: &str, format: Format) -> io::Result<()> {
        let name = c_string(name)?;
        // SAFETY: handle is live; the name is copied by mpv.
        check(&self.api, unsafe {
            (self.api.observe_property)(self.handle, id, name.as_ptr(), format.code())
        })?;
        Ok(())
    }

    /// Returns the next queued event without blocking.
    pub fn poll(&self) -> Option<Event> {
        loop {
            // SAFETY: handle is live; a zero timeout returns immediately with a record valid until the next call.
            let record = unsafe { &*(self.api.wait_event)(self.handle, 0.0) };
            let event = match record.id {
                ffi::EVENT_NONE => return None,
                ffi::EVENT_SHUTDOWN => Event::Shutdown,
                ffi::EVENT_START_FILE => Event::StartFile,
                ffi::EVENT_FILE_LOADED => Event::FileLoaded,
                ffi::EVENT_END_FILE => Event::EndFile(self.end_reason(record)),
                ffi::EVENT_SEEK => Event::Seek,
                ffi::EVENT_PLAYBACK_RESTART => Event::PlaybackRestart,
                ffi::EVENT_VIDEO_RECONFIG => Event::VideoReconfig,
                ffi::EVENT_AUDIO_RECONFIG => Event::AudioReconfig,
                ffi::EVENT_QUEUE_OVERFLOW => Event::QueueOverflow,
                ffi::EVENT_PROPERTY_CHANGE => Event::Property {
                    id: record.reply_userdata,
                    name: Self::property_name(record),
                    value: Self::property_value(record),
                },
                // Log, tick, hook and client-message events are not subscribed to; skip without surfacing.
                _ => continue,
            };
            return Some(event);
        }
    }

    fn end_reason(&self, record: &EventRecord) -> EndReason {
        if record.data.is_null() {
            return EndReason::Stopped;
        }
        // SAFETY: for MPV_EVENT_END_FILE `data` points to an mpv_event_end_file whose first fields are
        // `reason` and `error` (both c_int).
        let (reason, error) = unsafe {
            let data = record.data.cast::<i32>();
            (*data, *data.add(1))
        };
        match reason {
            ffi::END_EOF => EndReason::Finished,
            ffi::END_QUIT => EndReason::Quit,
            ffi::END_ERROR => EndReason::Failed(describe(&self.api, error)),
            ffi::END_REDIRECT => EndReason::Redirected,
            _ => EndReason::Stopped,
        }
    }

    fn property_name(record: &EventRecord) -> String {
        if record.data.is_null() {
            return String::new();
        }
        // SAFETY: for MPV_EVENT_PROPERTY_CHANGE `data` points to an mpv_event_property.
        let property = unsafe { &*record.data.cast::<ffi::EventProperty>() };
        // SAFETY: `name` is a NUL-terminated string valid until the next wait_event call.
        unsafe { CStr::from_ptr(property.name) }
            .to_string_lossy()
            .into_owned()
    }

    fn property_value(record: &EventRecord) -> Value {
        if record.data.is_null() {
            return Value::None;
        }
        // SAFETY: as in `property_name`.
        let property = unsafe { &*record.data.cast::<ffi::EventProperty>() };
        if property.data.is_null() {
            return Value::None;
        }
        // SAFETY: `data` has the layout of `format`: int (flag), int64, double or char*.
        unsafe {
            match property.format {
                ffi::FORMAT_FLAG => Value::Flag(*property.data.cast::<i32>() != 0),
                ffi::FORMAT_INT64 => Value::Int(*property.data.cast::<i64>()),
                ffi::FORMAT_DOUBLE => Value::Double(*property.data.cast::<f64>()),
                ffi::FORMAT_STRING => {
                    let text = *property.data.cast::<*const c_char>();
                    if text.is_null() {
                        Value::None
                    } else {
                        Value::Text(CStr::from_ptr(text).to_string_lossy().into_owned())
                    }
                }
                _ => Value::None,
            }
        }
    }

    /// Whether mpv has a new video frame that [`Mpv::render`] should draw.
    pub fn render_ready(&self) -> bool {
        // SAFETY: the render context is live for the lifetime of `self`.
        unsafe { (self.api.render_update)(self.render) & ffi::UPDATE_FRAME != 0 }
    }

    /// Renders the current frame, with subtitles and OSD, into `pixels` (`width * height`, row-major,
    /// bytes B,G,R,0 per pixel so the `u32` is `0x00RRGGBB`).
    ///
    /// # Errors
    /// `InvalidInput` when the buffer length is not `width * height` or a side is zero; otherwise mpv's error.
    pub fn render(&self, width: u32, height: u32, pixels: &mut [u32]) -> io::Result<()> {
        if width == 0 || height == 0 || pixels.len() != width as usize * height as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame buffer does not match its size",
            ));
        }
        let mut size = [
            i32::try_from(width).map_err(io::Error::other)?,
            i32::try_from(height).map_err(io::Error::other)?,
        ];
        let mut stride = width as usize * 4;
        let mut params = [
            Param {
                kind: ffi::PARAM_SW_SIZE,
                data: size.as_mut_ptr().cast(),
            },
            Param {
                kind: ffi::PARAM_SW_FORMAT,
                data: c"bgr0".as_ptr().cast_mut().cast(),
            },
            Param {
                kind: ffi::PARAM_SW_STRIDE,
                data: ptr::addr_of_mut!(stride).cast(),
            },
            Param {
                kind: ffi::PARAM_SW_POINTER,
                data: pixels.as_mut_ptr().cast(),
            },
            Param {
                kind: ffi::PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: the buffer holds `stride * height` writable bytes and every param outlives the call.
        check(&self.api, unsafe {
            (self.api.render_render)(self.render, params.as_mut_ptr())
        })?;
        Ok(())
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: callbacks are cleared first so no mpv thread touches `wake` afterwards; the render context
        // must be freed before the core, and the core before the library is unmapped.
        unsafe {
            (self.api.set_wakeup_callback)(self.handle, None, ptr::null_mut());
            if !self.render.is_null() {
                (self.api.render_set_update_callback)(self.render, None, ptr::null_mut());
                (self.api.render_free)(self.render);
            }
            (self.api.terminate_destroy)(self.handle);
        }
    }
}
