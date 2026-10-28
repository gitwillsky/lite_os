//! Function-pointer table and C layouts of mpv 0.40's client.h / render.h.

use linux_uapi::dynamic_library::Library;
use std::{
    ffi::{c_char, c_int, c_void},
    io,
};

pub(crate) enum Handle {}
pub(crate) enum RenderContext {}

pub(crate) const FORMAT_STRING: c_int = 1;
pub(crate) const FORMAT_FLAG: c_int = 3;
pub(crate) const FORMAT_INT64: c_int = 4;
pub(crate) const FORMAT_DOUBLE: c_int = 5;

pub(crate) const EVENT_NONE: c_int = 0;
pub(crate) const EVENT_SHUTDOWN: c_int = 1;
pub(crate) const EVENT_START_FILE: c_int = 6;
pub(crate) const EVENT_END_FILE: c_int = 7;
pub(crate) const EVENT_FILE_LOADED: c_int = 8;
pub(crate) const EVENT_VIDEO_RECONFIG: c_int = 17;
pub(crate) const EVENT_AUDIO_RECONFIG: c_int = 18;
pub(crate) const EVENT_SEEK: c_int = 20;
pub(crate) const EVENT_PLAYBACK_RESTART: c_int = 21;
pub(crate) const EVENT_PROPERTY_CHANGE: c_int = 22;
pub(crate) const EVENT_QUEUE_OVERFLOW: c_int = 24;

pub(crate) const END_EOF: c_int = 0;
pub(crate) const END_QUIT: c_int = 3;
pub(crate) const END_ERROR: c_int = 4;
pub(crate) const END_REDIRECT: c_int = 5;

pub(crate) const PARAM_INVALID: c_int = 0;
pub(crate) const PARAM_API_TYPE: c_int = 1;
pub(crate) const PARAM_SW_SIZE: c_int = 17;
pub(crate) const PARAM_SW_FORMAT: c_int = 18;
pub(crate) const PARAM_SW_STRIDE: c_int = 19;
pub(crate) const PARAM_SW_POINTER: c_int = 20;
pub(crate) const UPDATE_FRAME: u64 = 1;

/// `mpv_event`.
#[repr(C)]
pub(crate) struct EventRecord {
    pub id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

/// `mpv_event_property`.
#[repr(C)]
pub(crate) struct EventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

/// `mpv_render_param`.
#[repr(C)]
pub(crate) struct Param {
    pub kind: c_int,
    pub data: *mut c_void,
}

type Callback = Option<extern "C" fn(*mut c_void)>;

/// Resolved entry points; valid while the [`Library`] they came from stays open.
pub(crate) struct Api {
    pub create: unsafe extern "C" fn() -> *mut Handle,
    pub initialize: unsafe extern "C" fn(*mut Handle) -> c_int,
    pub terminate_destroy: unsafe extern "C" fn(*mut Handle),
    pub set_option_string: unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    pub set_property_string:
        unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    pub get_property_string: unsafe extern "C" fn(*mut Handle, *const c_char) -> *mut c_char,
    pub get_property: unsafe extern "C" fn(*mut Handle, *const c_char, c_int, *mut c_void) -> c_int,
    pub free: unsafe extern "C" fn(*mut c_void),
    pub command: unsafe extern "C" fn(*mut Handle, *mut *const c_char) -> c_int,
    pub observe_property: unsafe extern "C" fn(*mut Handle, u64, *const c_char, c_int) -> c_int,
    pub wait_event: unsafe extern "C" fn(*mut Handle, f64) -> *mut EventRecord,
    pub set_wakeup_callback: unsafe extern "C" fn(*mut Handle, Callback, *mut c_void),
    pub error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    pub render_create:
        unsafe extern "C" fn(*mut *mut RenderContext, *mut Handle, *mut Param) -> c_int,
    pub render_set_update_callback: unsafe extern "C" fn(*mut RenderContext, Callback, *mut c_void),
    pub render_update: unsafe extern "C" fn(*mut RenderContext) -> u64,
    pub render_render: unsafe extern "C" fn(*mut RenderContext, *mut Param) -> c_int,
    pub render_free: unsafe extern "C" fn(*mut RenderContext),
}

impl Api {
    /// Resolves every entry point.
    ///
    /// # Safety
    /// `library` must be libmpv 0.40 (client API 2.x), whose signatures the fields above mirror.
    pub(crate) unsafe fn load(library: &Library) -> io::Result<Self> {
        // SAFETY: each field type is the extern "C" signature of the symbol it is resolved from.
        unsafe {
            Ok(Self {
                create: library.symbol("mpv_create")?,
                initialize: library.symbol("mpv_initialize")?,
                terminate_destroy: library.symbol("mpv_terminate_destroy")?,
                set_option_string: library.symbol("mpv_set_option_string")?,
                set_property_string: library.symbol("mpv_set_property_string")?,
                get_property_string: library.symbol("mpv_get_property_string")?,
                get_property: library.symbol("mpv_get_property")?,
                free: library.symbol("mpv_free")?,
                command: library.symbol("mpv_command")?,
                observe_property: library.symbol("mpv_observe_property")?,
                wait_event: library.symbol("mpv_wait_event")?,
                set_wakeup_callback: library.symbol("mpv_set_wakeup_callback")?,
                error_string: library.symbol("mpv_error_string")?,
                render_create: library.symbol("mpv_render_context_create")?,
                render_set_update_callback: library
                    .symbol("mpv_render_context_set_update_callback")?,
                render_update: library.symbol("mpv_render_context_update")?,
                render_render: library.symbol("mpv_render_context_render")?,
                render_free: library.symbol("mpv_render_context_free")?,
            })
        }
    }
}
