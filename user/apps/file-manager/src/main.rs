//! The LiteOS file manager.
//!
//! A thin binary that runs the generic `lite-runtime` GUI/JS runtime with no
//! extensions — the file manager needs only the library's built-in `fs.*`
//! bridge. The React UI is bundled to `/usr/share/liteos/apps/file-manager`.

use std::process::exit;

use lite_runtime::Role;

fn main() {
    if let Err(error) = lite_runtime::run(Role::App, "file-manager", Vec::new()) {
        eprintln!("file-manager: {error}");
        exit(1);
    }
}
