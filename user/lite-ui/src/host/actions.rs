//! Deferred native side effects executed after one JavaScript turn.

/// One side effect requested synchronously by React and executed after its JS turn.
pub enum Action {
    /// Launch one checked application registry id.
    Launch(String),
    /// Route a desktop-owned app configure.
    Configure {
        surface_id: u32,
        serial: u64,
        width: u32,
        height: u32,
    },
    /// Route an unconditional app close.
    Close(u32),
    /// Authorize one compositor-side move from an exact pointer-down serial.
    BeginMove {
        surface_id: u32,
        serial: u64,
        min_x: i32,
        min_y: i32,
        max_x: i32,
        max_y: i32,
    },
    /// Atomically replace the compositor's global accelerator table.
    SetAccelerators(Vec<display_proto::AcceleratorChord>),
    /// Request system shutdown.
    Shutdown,
    /// Send bytes to the terminal helper.
    TerminalInput(Vec<u8>),
    /// Request one asynchronous standard Clipboard API read.
    ClipboardRead(u64),
    /// Publish complete UTF-8 text through the standard Clipboard API.
    ClipboardWrite(String),
    /// Paste complete UTF-8 text into the terminal PTY.
    TerminalPaste(String),
}
