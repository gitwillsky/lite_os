//! Client for the upstream libmpv: properties, commands, events and the software render API.
//!
//! The shared object is opened at run time ([`linux_uapi::dynamic_library`]); nothing links against it.
//! The control callback wakes the UI through a non-blocking socket; render callbacks feed a dedicated
//! render thread, which services render updates independently of synchronous control calls.

extern crate liteos_runtime as _;

mod ffi;
mod render;

use ffi::{Api, EventRecord, Handle};
use linux_uapi::dynamic_library::Library;
use render::Renderer;
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
    render: Option<Renderer>,
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
        let render_writer = writer.try_clone()?;
        // SAFETY: mpv_create has no preconditions; a null result means the core could not be allocated.
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err(io::Error::other("mpv_create failed"));
        }
        let mut player = Self {
            api,
            handle,
            render: None,
            wake: Box::new(Wake { writer }),
            _library: library,
        };
        for (name, value) in options {
            player.set_option(name, value)?;
        }
        // SAFETY: handle is live and not yet initialised.
        check(&player.api, unsafe { (player.api.initialize)(handle) })?;
        // SAFETY: player 在 Renderer 停止并 join 后才销毁 handle 和 library。
        player.render = Some(unsafe { Renderer::start(api, handle, render_writer)? });
        let data = ptr::addr_of!(*player.wake).cast_mut().cast::<c_void>();
        // SAFETY: Wake 地址稳定，Drop 在释放它之前先撤销 callback。
        unsafe { (api.set_wakeup_callback)(handle, Some(wake), data) };
        Ok((player, reader))
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
        self.render.as_ref().is_some_and(Renderer::ready)
    }

    /// Renders the current frame on its dedicated render thread and returns the reused BGR0 buffer.
    /// The buffer remains borrowed until the caller finishes copying it into its UI surface.
    ///
    /// # Errors
    /// Invalid dimensions, a stopped render thread, or an upstream rendering failure.
    pub fn render(&mut self, width: u32, height: u32) -> io::Result<&[u32]> {
        self.render
            .as_mut()
            .ok_or_else(|| io::Error::other("mpv renderer is unavailable"))?
            .render(width, height)
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: Wake remains live until callback removal completes.
        unsafe { (self.api.set_wakeup_callback)(self.handle, None, ptr::null_mut()) };
        // The render context and its callback must be gone before the core or shared object.
        drop(self.render.take());
        // SAFETY: no render thread or callback can use the handle after the join above.
        unsafe { (self.api.terminate_destroy)(self.handle) };
    }
}
