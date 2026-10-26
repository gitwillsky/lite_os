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

/// `user/programs/` 下的可执行程序包；每个都有 `src/main.rs`。
const PROGRAMS: [&str; 12] = [
    "compositor",
    "logon",
    "desktop",
    "file-explorer",
    "control-panel",
    "task-manager",
    "volume-control",
    "terminal",
    "sound-player",
    "media-player",
    "notepad",
    "device-manager",
];

/// `user/libraries/` 下的库包；每个都有 `src/lib.rs`。其中带 `liteos-runtime` 依赖的会被构建成共享库。
const LIBRARIES: [&str; 20] = [
    "calendar",
    "canvas",
    "classic-theme",
    "classic-ui",
    "classic-ui-abi",
    "classic-ui-runtime",
    "liteos-ui",
    "desktop-entry",
    "desktop-settings",
    "keyboard",
    "linux-uapi",
    "liteos-runtime",
    "network-config",
    "sndvol",
    "sound",
    "terminal-font",
    "terminal-model",
    "typeface",
    "wayland-scanner",
    "wayland-wire",
];

fn names_in(root: &Path, directory: &str) -> BTreeSet<String> {
    fs::read_dir(root.join("user").join(directory))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn check_user_tree(root: &Path, errors: &mut Vec<String>) {
    let expected_top: BTreeSet<String> = [
        "Cargo.lock",
        "Cargo.toml",
        "README.md",
        "base",
        "diagnostics",
        "libraries",
        "programs",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let actual_top = fs::read_dir(root.join("user"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name() != "target")
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if actual_top != expected_top {
        errors.push(format!(
            "user/: expected {expected_top:?} (programs go in programs/, libraries in libraries/), found {actual_top:?}"
        ));
    }
    for (directory, expected, entry) in [
        ("programs", PROGRAMS.as_slice(), "src/main.rs"),
        ("libraries", LIBRARIES.as_slice(), "src/lib.rs"),
    ] {
        let expected_names: BTreeSet<String> = expected.iter().map(|n| (*n).to_owned()).collect();
        let actual = names_in(root, directory);
        if actual != expected_names {
            errors.push(format!(
                "user/{directory}/: expected {expected_names:?}, found {actual:?}"
            ));
        }
        for name in expected {
            let base = root.join("user").join(directory).join(name);
            if !base.join(entry).is_file() {
                errors.push(format!(
                    "user/{directory}/{name}/{entry}: required entry is missing"
                ));
            }
            if !base.join("README.md").is_file() {
                errors.push(format!(
                    "user/{directory}/{name}/README.md: every package states whether it is a program or a library and what it owns"
                ));
            }
        }
        if !root
            .join("user")
            .join(directory)
            .join("README.md")
            .is_file()
        {
            errors.push(format!(
                "user/{directory}/README.md: the directory must be indexed"
            ));
        }
    }
}

fn check_workspace(root: &Path, errors: &mut Vec<String>) {
    let user = fs::read_to_string(root.join("user/Cargo.toml")).unwrap_or_default();
    let members = PROGRAMS
        .iter()
        .map(|n| format!("\"programs/{n}\""))
        .chain(LIBRARIES.iter().map(|n| format!("\"libraries/{n}\"")))
        .chain([
            "linux-uapi = { path = \"libraries/linux-uapi\" }".to_owned(),
            "unicode-width = \"=0.2.2\"".to_owned(),
        ]);
    for required in members {
        if !user.contains(&required) {
            errors.push(format!("user/Cargo.toml: missing `{required}`"));
        }
    }
    let root_workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    for excluded in PROGRAMS
        .iter()
        .map(|n| format!("\"user/programs/{n}\""))
        .chain(LIBRARIES.iter().map(|n| format!("\"user/libraries/{n}\"")))
    {
        if !root_workspace.contains(&excluded) {
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
        // 精确列出的 SDK seam 只拥有进程内稳定 ABI，不能导入 libc/设备符号。
        // Linux 原始 FFI 仍只属于 linux-uapi，不对整个 UI 目录开放例外。
        let sdk_abi = [
            "user/libraries/classic-ui-abi/src/lib.rs",
            "user/libraries/classic-ui-runtime/src/lib.rs",
            "user/libraries/classic-ui-runtime/src/context_api.rs",
            "user/libraries/classic-ui-runtime/src/window_api.rs",
            "user/libraries/classic-ui-runtime/src/widget_api.rs",
            "user/libraries/classic-ui-runtime/src/dialog_api.rs",
            "user/libraries/classic-ui-runtime/src/toplevel.rs",
            "user/libraries/classic-ui-runtime/src/outline.rs",
            "user/libraries/classic-ui-runtime/src/list_view.rs",
            "user/libraries/classic-ui-runtime/src/label_edit.rs",
            "user/libraries/liteos-ui/src/connection.rs",
        ]
        .iter()
        .any(|path| source == root.join(path));
        if sdk_abi {
            let text = fs::read_to_string(&source).unwrap_or_default();
            if let Ok(file) = syn::parse_file(&text) {
                for item in file.items {
                    if let syn::Item::ForeignMod(block) = item {
                        let sdk_binding =
                            source == root.join("user/libraries/liteos-ui/src/connection.rs");
                        if !sdk_binding
                            || block.items.len() != 1
                            || !matches!(
                                block.items.first(), Some(syn::ForeignItem::Fn(function)) if function.sig.ident == "liteos_ui_get_api"
                            )
                        {
                            errors.push(format!("{}: UI ABI may import only liteos_ui_get_api; OS/device FFI belongs to linux-uapi", source.display()));
                        }
                    }
                }
            } else {
                errors.push(format!(
                    "{}: UI ABI seam must parse as Rust",
                    source.display()
                ));
            }
        }
        let allowed = source.starts_with(root.join("user/libraries/linux-uapi")) || sdk_abi;
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
