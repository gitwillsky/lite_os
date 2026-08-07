//! The LiteOS desktop shell.
//!
//! A thin binary that runs the generic `lite-runtime` GUI/JS runtime as the
//! desktop role, with one [`DesktopExt`] extension that owns desktop *policy*:
//! - `apps.list`   — scan the installed-application registry (`<apps_root>/*/app.json`).
//! - `apps.launch` — launch a checked app as its own `/bin/<id>` binary.
//! - `desktop.shutdown` / `desktop.restart` — request an explicit system power action.
//!
//! The surface *mechanism* (`desktop.surfaces/configure/move/focus/close`,
//! accelerators) and `audio-system.*` volume control remain in the runtime
//! library as Role::Desktop compositor-client infrastructure — they are thin
//! wrappers over the runtime's own surface state, display action queue, and
//! audio command channel, not application logic.

mod registry;

use std::process::exit;

use lite_runtime::{Action, EngineError, ExtensionCx, HostExtension, Role};

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("desktop: invariant failure: {info}");
    }));
    if let Err(error) = lite_runtime::run(Role::Desktop, "", vec![Box::new(DesktopExt)]) {
        eprintln!("desktop: {error}");
        exit(1);
    }
}

/// The desktop shell's launcher/policy extension.
struct DesktopExt;

impl HostExtension for DesktopExt {
    fn invoke(
        &mut self,
        cx: &ExtensionCx,
        operation: &str,
        payload: &str,
    ) -> Option<Result<String, EngineError>> {
        match operation {
            "apps.list" => Some(Ok(registry::scan_apps(cx.apps_root()))),
            "apps.launch" => Some(self.launch(cx, payload)),
            "desktop.shutdown" => {
                cx.push_action(Action::Shutdown);
                Some(Ok(String::new()))
            }
            "desktop.restart" => {
                cx.push_action(Action::Restart);
                Some(Ok(String::new()))
            }
            _ => None,
        }
    }
}

impl DesktopExt {
    /// Launches one checked application id as its own `/bin/<id>` process.
    fn launch(&self, cx: &ExtensionCx, payload: &str) -> Result<String, EngineError> {
        if !registry::valid_app_id(payload) {
            return Err(EngineError::from_host("invalid application id"));
        }
        cx.push_action(Action::Launch(payload.to_owned()));
        Ok(String::new())
    }
}
