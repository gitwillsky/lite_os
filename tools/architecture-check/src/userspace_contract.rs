use std::{collections::BTreeSet, fs, path::Path};

use super::rust_files;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    check_user_tree(root, errors);
    check_workspace(root, errors);
    check_ffi_owners(root, errors);
    check_boot_route(root, errors);
    check_ui_product(root, errors);
    check_assets(root, errors);
}

fn check_user_tree(root: &Path, errors: &mut Vec<String>) {
    let expected = BTreeSet::from([
        "Cargo.lock",
        "Cargo.toml",
        "README.md",
        "base",
        "compositor",
        "diagnostics",
        "display-proto",
        "linux-uapi",
        "lite-ui",
        "quickjs-runtime",
        "terminal-session",
    ])
    .into_iter()
    .map(str::to_owned)
    .collect();
    let actual = fs::read_dir(root.join("user"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name() != "target")
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if actual != expected {
        errors.push(format!(
            "user/: expected the single LiteUI product track {expected:?}, found {actual:?}"
        ));
    }
    for forbidden in ["desktop", "splash", "terminal"] {
        if root.join("user").join(forbidden).exists() {
            errors.push(format!(
                "user/{forbidden}: obsolete GUI track must be removed"
            ));
        }
    }
    for required in [
        "compositor/src/lib.rs",
        "compositor/src/boot.rs",
        "compositor/src/scanout.rs",
        "compositor/src/session.rs",
        "display-proto/src/lib.rs",
        "display-proto/src/scene.rs",
        "lite-ui/src/main.rs",
        "lite-ui/src/renderer.rs",
        "quickjs-runtime/src/raw.rs",
        "quickjs-runtime/vendor/quickjs/quickjs.c",
        "terminal-session/src/lib.rs",
        "terminal-session/src/model.rs",
    ] {
        if !root.join("user").join(required).is_file() {
            errors.push(format!(
                "user/{required}: required product owner is missing"
            ));
        }
    }
}

fn check_workspace(root: &Path, errors: &mut Vec<String>) {
    let user = fs::read_to_string(root.join("user/Cargo.toml")).unwrap_or_default();
    for required in [
        "members = [\"compositor\", \"display-proto\", \"linux-uapi\", \"lite-ui\", \"quickjs-runtime\", \"terminal-session\"]",
        "quickjs-runtime = { path = \"quickjs-runtime\" }",
        "cssparser = \"=0.37.0\"",
        "taffy = \"=0.12.2\"",
        "tiny-skia = \"=0.12.0\"",
        "version = \"=0.11.0\"",
    ] {
        if !user.contains(required) {
            errors.push(format!("user/Cargo.toml: missing `{required}`"));
        }
    }
    let root_workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    for excluded in [
        "\"user/compositor\"",
        "\"user/display-proto\"",
        "\"user/linux-uapi\"",
        "\"user/lite-ui\"",
        "\"user/quickjs-runtime\"",
        "\"user/terminal-session\"",
    ] {
        if !root_workspace.contains(excluded) {
            errors.push(format!(
                "Cargo.toml: workspace exclude is missing {excluded}"
            ));
        }
    }
}

fn check_ffi_owners(root: &Path, errors: &mut Vec<String>) {
    let mut sources = Vec::new();
    if let Err(error) = rust_files(&root.join("user"), &mut sources) {
        errors.push(error);
        return;
    }
    for source in sources {
        let allowed = source.starts_with(root.join("user/linux-uapi"))
            || source == root.join("user/quickjs-runtime/src/raw.rs");
        if allowed {
            continue;
        }
        let text = fs::read_to_string(&source).unwrap_or_default();
        if text.contains("extern \"C\"") || text.contains("#[link(") {
            errors.push(format!(
                "{}: raw FFI belongs only to linux-uapi or quickjs-runtime/raw.rs",
                source.display()
            ));
        }
    }
}

fn check_boot_route(root: &Path, errors: &mut Vec<String>) {
    let inittab = fs::read_to_string(root.join("user/base/inittab")).unwrap_or_default();
    let expected = "::once:/etc/init.d/graphical-session /bin/compositor\n::once:/etc/init.d/graphical-session /bin/lite-ui --desktop\n::respawn:/etc/init.d/network-service\n::respawn:-/bin/sh\n";
    if inittab != expected {
        errors.push(
            "user/base/inittab: must supervise compositor, React desktop, network and UART recovery exactly once"
                .to_owned(),
        );
    }
    let graphical =
        fs::read_to_string(root.join("user/base/graphical-session")).unwrap_or_default();
    for required in ["/bin/compositor --probe", "while :", "\"$@\""] {
        if !graphical.contains(required) {
            errors.push(format!("user/base/graphical-session: missing `{required}`"));
        }
    }
    let builder = fs::read_to_string(root.join("scripts/verify_busybox.py")).unwrap_or_default();
    for required in [
        "def build_compositor(",
        "def build_lite_ui(",
        "def build_terminal_session(",
        "def build_ui_assets(",
        "/bin/compositor",
        "/bin/lite-ui",
        "/bin/terminal-session",
        "/usr/lib/lite-ui/runtime.js",
        "/usr/share/liteos/desktop/main.js",
        "/usr/share/liteos/apps/terminal/app.json",
    ] {
        if !builder.contains(required) {
            errors.push(format!("scripts/verify_busybox.py: missing `{required}`"));
        }
    }
    for forbidden in [
        "/bin/desktop",
        "/bin/splash",
        "startmenu.conf",
        "wallpaper.xrgb",
    ] {
        if builder.contains(forbidden) {
            errors.push(format!(
                "scripts/verify_busybox.py: obsolete product `{forbidden}` remains"
            ));
        }
    }
}

fn check_ui_product(root: &Path, errors: &mut Vec<String>) {
    let package = fs::read_to_string(root.join("ui/package.json")).unwrap_or_default();
    for required in [
        "\"react\": \"19.2.7\"",
        "\"react-reconciler\": \"0.33.0\"",
        "\"esbuild\": \"0.28.1\"",
    ] {
        if !package.contains(required) {
            errors.push(format!("ui/package.json: missing `{required}`"));
        }
    }
    for required in [
        "ui/build.mjs",
        "ui/package-lock.json",
        "ui/src/runtime/renderer.js",
        "ui/src/design-system/window.jsx",
        "ui/src/design-system/taskbar.jsx",
        "ui/src/desktop/main.jsx",
        "ui/src/desktop/style.css",
        "ui/src/terminal/main.jsx",
        "ui/src/terminal/app.json",
    ] {
        if !root.join(required).is_file() {
            errors.push(format!(
                "{required}: required React product source is missing"
            ));
        }
    }
}

fn check_assets(root: &Path, errors: &mut Vec<String>) {
    let ui_atlas = fs::read(root.join("assets/fonts/liteos-ui.a8p")).unwrap_or_default();
    if ui_atlas.get(..8) != Some(b"LUP8\0\0\0\x01") || ui_atlas.len() != 7_458_232 {
        errors.push("assets/fonts/liteos-ui.a8p: checked UI atlas identity changed".to_owned());
    }
    let bootlogo = fs::read(root.join("assets/bootlogo.xrgb")).unwrap_or_default();
    if bootlogo.get(..8) != Some(b"LWP8\0\0\0\x01") || bootlogo.len() != 3_145_744 {
        errors.push("assets/bootlogo.xrgb: checked boot scene identity changed".to_owned());
    }
    for removed in [
        "assets/wallpaper.xrgb",
        "assets/desktop-sprites.argb",
        "user/base/startmenu.conf",
    ] {
        if root.join(removed).exists() {
            errors.push(format!(
                "{removed}: obsolete native-shell asset must be removed"
            ));
        }
    }
}
