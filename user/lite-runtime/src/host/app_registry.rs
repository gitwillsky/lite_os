//! Window metadata for compositor surfaces the desktop opens.
//!
//! This is the small, generic slice of app metadata the *runtime* itself needs:
//! when a compositor surface connects,
//! [`State::open_surface`](crate::host::State::open_surface) labels it with a
//! title and icon for the desktop's window chrome. The desktop *launcher*
//! registry (scanning `<apps_root>/*/app.json`, id validation, icon
//! normalization) is app policy and lives in the desktop binary's extension,
//! not here.

/// The window title and chrome icon for a known app id (fallback for unknown).
pub(super) fn app_metadata(id: &str) -> (&'static str, &'static str) {
    match id {
        "terminal" => ("Terminal", "assets/terminal.png"),
        "my-computer" => ("Computer", "assets/package.png"),
        "file-manager" => ("Files", "assets/files.png"),
        "music-player" => ("Music", "assets/music.png"),
        _ => ("Application", "assets/terminal.png"),
    }
}
