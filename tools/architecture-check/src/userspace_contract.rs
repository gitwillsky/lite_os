use std::{collections::BTreeSet, fs, path::Path};

use super::rust_files;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    check_user_tree(root, errors);
    check_workspace(root, errors);
    check_ffi_owners(root, errors);
    check_boot_route(root, errors);
    check_aarch64_el0_execution_access(root, errors);
    check_graphics_device_path(root, errors);
    check_assets(root, errors);
}

fn check_aarch64_el0_execution_access(root: &Path, errors: &mut Vec<String>) {
    let source =
        fs::read_to_string(root.join("kernel/src/arch/aarch64/startup.rs")).unwrap_or_default();
    for required in [
        "const EL0_VIRTUAL_COUNTER_ACCESS: u64 = 1 << 1;",
        "const EL0_CACHE_ACCESS: u64 = (1 << 26) | (1 << 15) | (1 << 14);",
        "\"msr cntkctl_el1, {counter_access}\"",
        "counter_access = in(reg) EL0_VIRTUAL_COUNTER_ACCESS",
        "\"msr sctlr_el1, x10\"",
        "cache_access = in(reg) EL0_CACHE_ACCESS",
    ] {
        if !source.contains(required) {
            errors.push(format!(
                "kernel/src/arch/aarch64/startup.rs: EL0 must receive the standard virtual-counter and cache-execution controls via `{required}`"
            ));
        }
    }
}

fn check_user_tree(root: &Path, errors: &mut Vec<String>) {
    let expected = BTreeSet::from([
        "Cargo.lock",
        "Cargo.toml",
        "README.md",
        "base",
        "canvas",
        "classic-theme",
        "classic-ui",
        "compositor",
        "diagnostics",
        "keyboard",
        "linux-uapi",
        "terminal-font",
        "terminal-model",
        "typeface",
        "wayland-scanner",
        "wayland-wire",
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
            "user/: expected the UART baseline and retained standard libraries {expected:?}, found {actual:?}"
        ));
    }
    for required in [
        "canvas/src/lib.rs",
        "classic-theme/src/lib.rs",
        "classic-ui/src/lib.rs",
        "compositor/src/main.rs",
        "keyboard/src/lib.rs",
        "linux-uapi/src/lib.rs",
        "terminal-font/src/lib.rs",
        "terminal-model/src/lib.rs",
        "typeface/src/lib.rs",
        "wayland-scanner/src/lib.rs",
        "wayland-wire/src/lib.rs",
    ] {
        if !root.join("user").join(required).is_file() {
            errors.push(format!(
                "user/{required}: required library owner is missing"
            ));
        }
    }
}

fn check_workspace(root: &Path, errors: &mut Vec<String>) {
    let user = fs::read_to_string(root.join("user/Cargo.toml")).unwrap_or_default();
    for required in [
        "\"canvas\"",
        "\"classic-theme\"",
        "\"classic-ui\"",
        "\"compositor\"",
        "\"keyboard\"",
        "\"linux-uapi\"",
        "\"terminal-font\"",
        "\"terminal-model\"",
        "\"typeface\"",
        "\"wayland-scanner\"",
        "\"wayland-wire\"",
        "linux-uapi = { path = \"linux-uapi\" }",
        "unicode-width = \"=0.2.2\"",
    ] {
        if !user.contains(required) {
            errors.push(format!("user/Cargo.toml: missing `{required}`"));
        }
    }
    let root_workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    for excluded in [
        "\"user/canvas\"",
        "\"user/classic-theme\"",
        "\"user/classic-ui\"",
        "\"user/compositor\"",
        "\"user/keyboard\"",
        "\"user/linux-uapi\"",
        "\"user/terminal-font\"",
        "\"user/terminal-model\"",
        "\"user/typeface\"",
        "\"user/wayland-scanner\"",
        "\"user/wayland-wire\"",
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
        let allowed = source.starts_with(root.join("user/linux-uapi"));
        if allowed {
            continue;
        }
        let text = fs::read_to_string(&source).unwrap_or_default();
        if text.contains("extern \"C\"") || text.contains("#[link(") {
            errors.push(format!(
                "{}: raw FFI belongs only to linux-uapi",
                source.display()
            ));
        }
    }
}

fn check_boot_route(root: &Path, errors: &mut Vec<String>) {
    let inittab = fs::read_to_string(root.join("user/base/inittab")).unwrap_or_default();
    let expected = "::sysinit:/etc/init.d/rcS\n::respawn:/etc/init.d/network-service\n::respawn:/etc/init.d/display-service\n::respawn:-/bin/sh\n";
    if inittab != expected {
        errors.push(
            "user/base/inittab: must mount pseudo filesystems through rcS first, then supervise network, display and UART shell exactly once"
                .to_owned(),
        );
    }
    let builder = fs::read_to_string(root.join("scripts/verify_busybox.py")).unwrap_or_default();
    for required in [
        "def base_userland(",
        "user/base/inittab",
        "user/base/rcS",
        "user/base/profile",
        "boot_scene()",
        "cursors()",
    ] {
        if !builder.contains(required) {
            errors.push(format!("scripts/verify_busybox.py: missing `{required}`"));
        }
    }
    for forbidden in [
        "build_ui_assets",
        "build_graphical_userland",
        "/bin/audio-service",
        "ui/package.json",
    ] {
        if builder.contains(forbidden) {
            errors.push(format!(
                "scripts/verify_busybox.py: retired userland `{forbidden}` remains"
            ));
        }
    }
    if root.join("user/base/graphical-session").exists() || root.join("ui").exists() {
        errors.push("retired graphical session and React frontend must remain absent".to_owned());
    }
}

fn check_graphics_device_path(root: &Path, errors: &mut Vec<String>) {
    let required = [
        (
            "kernel/src/virtio/gpu.rs",
            "notify_queue(CURSOR_QUEUE)",
            "hardware cursor commands must use the dedicated VirtIO-GPU cursor queue",
        ),
        (
            "kernel/src/virtio/gpu.rs",
            "cursor.latest_move = Some((x, y));",
            "cursor motion must coalesce in the adapter instead of blocking userspace",
        ),
        (
            "scripts/utm_runtime.py",
            "agent-mouse=off",
            "UTM pointer input must stay on the sole virtio-tablet path",
        ),
    ];
    for (path, marker, failure) in required {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        if !source.contains(marker) {
            errors.push(format!("{path}: {failure}"));
        }
    }
}

fn check_assets(root: &Path, errors: &mut Vec<String>) {
    let terminal_atlas = fs::read(root.join("assets/fonts/liteos-terminal.a8")).unwrap_or_default();
    if terminal_atlas.get(..8) != Some(b"LTA8\0\0\0\x03") || terminal_atlas.len() != 9_172_472 {
        errors.push(
            "assets/fonts/liteos-terminal.a8: checked terminal atlas identity changed".to_owned(),
        );
    }
    let boot_scene = fs::read(root.join("assets/classic/boot-scene.lbs")).unwrap_or_default();
    if boot_scene.get(..4) != Some(b"LBS1") || boot_scene.len() != 30_416 {
        errors
            .push("assets/classic/boot-scene.lbs: checked boot scene identity changed".to_owned());
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

#[cfg(test)]
mod tests {
    #[test]
    fn repository_exposes_standard_aarch64_execution_controls_to_el0() {
        let root = super::super::repository_root();
        let mut errors = Vec::new();
        super::check_aarch64_el0_execution_access(&root, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
