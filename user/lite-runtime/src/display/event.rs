//! Validated asynchronous events exposed by the display protocol client.

use display_proto::{
    ClipboardData, Configure, InputKey, InputPointer, InputScroll, OutputConfigure,
};

/// One exact role-checked event consumed by the LiteUI event loop.
#[derive(Clone, Debug)]
pub enum Event {
    /// Ordinary app published a top-level surface.
    AppOpened { surface_id: u32, app_id: String },
    /// Ordinary app removed its top-level surface.
    AppClosed { surface_id: u32 },
    /// A pointer-down hit a foreign surface; the desktop should raise it.
    SurfaceActivated { surface_id: u32 },
    /// A compositor-side move ended at one canonical logical position. The
    /// `move_token` must be echoed in the scene commit that applies this move so
    /// the compositor retires the grab.
    MoveComplete {
        surface_id: u32,
        x: i32,
        y: i32,
        move_token: u64,
    },
    /// App pixels for one desktop configure are ready.
    ConfigureReady { surface_id: u32, serial: u64 },
    /// Desktop selected a new app client size.
    Configure(Configure),
    /// Compositor selected a new physical viewport for the desktop document.
    OutputConfigure(OutputConfigure),
    /// Desktop requested app termination.
    Close,
    /// Pointer input routed against the presented scene.
    Pointer(InputPointer),
    /// Mouse-wheel scroll routed against the presented scene.
    Scroll(InputScroll),
    /// Keyboard input routed to the presented focused surface.
    Key(InputKey),
    /// Result of one exact plain-text clipboard request.
    ClipboardData(ClipboardData),
    /// A submit or buffer-release acknowledgement advanced pipeline progress.
    FrameDone,
    /// The compositor completed the page flip for this document's latest frame.
    Presented {
        /// CLOCK_MONOTONIC presentation timestamp supplied by DRM.
        monotonic_ns: u64,
    },
}
