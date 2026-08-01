//! The LiteOS terminal.
//!
//! A thin binary that runs the generic `lite-runtime` GUI/JS runtime with no
//! HostExtension. The PTY/VT client is runtime-integrated I/O — it participates
//! in the runtime event loop (poll fd, screen dispatch on the `terminal`
//! channel, viewport resize, EOF-driven exit), so it lives in the library,
//! activated for the `terminal` app id. The React UI is bundled to
//! `/usr/share/liteos/apps/terminal`.

use std::process::exit;

use lite_runtime::Role;

fn main() {
    if let Err(error) = lite_runtime::run(Role::App, "terminal", Vec::new()) {
        eprintln!("terminal: {error}");
        exit(1);
    }
}
