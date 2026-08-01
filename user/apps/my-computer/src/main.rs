//! LiteOS "My Computer".
//!
//! A thin binary that runs the generic `lite-runtime` GUI/JS runtime with no
//! extensions — it uses only the library's built-in `fs.*` bridge. The React UI
//! is bundled to `/usr/share/liteos/apps/my-computer`.

use std::process::exit;

use lite_runtime::Role;

fn main() {
    if let Err(error) = lite_runtime::run(Role::App, "my-computer", Vec::new()) {
        eprintln!("my-computer: {error}");
        exit(1);
    }
}
