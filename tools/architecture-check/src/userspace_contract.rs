use std::{collections::BTreeSet, fs, path::Path};

use super::rust_files;

fn crate_sources(crate_dir: &Path, errors: &mut Vec<String>) -> BTreeSet<String> {
    let mut source_paths = Vec::new();
    if let Err(error) = rust_files(&crate_dir.join("src"), &mut source_paths) {
        errors.push(error);
    }
    source_paths
        .iter()
        .filter_map(|path| path.strip_prefix(crate_dir.join("src")).ok())
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<BTreeSet<_>>()
}

/// 校验单个用户态 crate 的目录形态、源文件清单与 manifest 必需项。
fn check_crate(
    root: &Path,
    name: &str,
    expected_sources: &[&str],
    required_manifest: &[&str],
    errors: &mut Vec<String>,
) {
    let crate_dir = root.join("user").join(name);
    let expected_entries = BTreeSet::from([
        "Cargo.lock".to_owned(),
        "Cargo.toml".to_owned(),
        "src".to_owned(),
    ]);
    let actual_entries = fs::read_dir(&crate_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if actual_entries != expected_entries {
        errors.push(format!(
            "user/{name}: expected exactly {expected_entries:?}, found {actual_entries:?}"
        ));
    }

    let expected = expected_sources
        .iter()
        .map(|source| (*source).to_owned())
        .collect::<BTreeSet<_>>();
    let actual = crate_sources(&crate_dir, errors);
    if actual != expected {
        errors.push(format!(
            "user/{name}/src: expected exactly {expected:?}, found {actual:?}"
        ));
    }

    let manifest = fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap_or_default();
    for required in required_manifest {
        if !manifest.contains(required) {
            errors.push(format!("user/{name}/Cargo.toml: missing `{required}`"));
        }
    }
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    let allowed = BTreeSet::from([
        "README.md",
        "base",
        "desktop",
        "diagnostics",
        "display-proto",
        "splash",
        "terminal",
    ]);
    let actual = match fs::read_dir(root.join("user")) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>(),
        Err(error) => {
            errors.push(format!("failed to inspect user/: {error}"));
            return;
        }
    };
    let expected = allowed
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if actual != expected {
        errors.push(format!(
            "user/: expected the desktop product track {expected:?}, found {actual:?}"
        ));
    }

    for (directory, names) in [
        (
            "base",
            &[
                "busybox.config",
                "group",
                "inittab",
                "liteos.terminfo",
                "network-service",
                "passwd",
                "shutdown",
                "startmenu.conf",
                "udhcpc.script",
            ][..],
        ),
        ("diagnostics", &["liteos-stress.c"][..]),
    ] {
        let expected = names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>();
        let actual = fs::read_dir(root.join("user").join(directory))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        if actual != expected {
            errors.push(format!(
                "user/{directory}: expected exactly {expected:?}, found {actual:?}"
            ));
        }
    }

    // console-session 已随桌面轨道正式移除；splash 是零依赖的 sysinit 启动画面 crate。
    check_crate(
        root,
        "splash",
        &["ffi.rs", "lib.rs", "render.rs"],
        &[
            "name = \"splash\"",
            "crate-type = [\"staticlib\"]",
            "panic = \"abort\"",
        ],
        errors,
    );
    let splash_manifest =
        fs::read_to_string(root.join("user/splash/Cargo.toml")).unwrap_or_default();
    if splash_manifest.contains("[dependencies]") || splash_manifest.contains(" path = ") {
        errors.push("user/splash: the splash Module must remain dependency-free".to_owned());
    }

    // 桌面轨道的三个 crate：desktop/terminal 只允许依赖 display-proto，display-proto 零依赖。
    check_crate(
        root,
        "display-proto",
        &["lib.rs", "message.rs", "transport.rs"],
        &["name = \"display-proto\""],
        errors,
    );
    let proto_manifest =
        fs::read_to_string(root.join("user/display-proto/Cargo.toml")).unwrap_or_default();
    if proto_manifest.contains("[dependencies]") || proto_manifest.contains(" path = ") {
        errors
            .push("user/display-proto: the protocol crate must remain dependency-free".to_owned());
    }
    for (name, sources) in [
        (
            "desktop",
            &[
                "chrome.rs",
                "clients.rs",
                "compositor.rs",
                "cursor.rs",
                "ffi.rs",
                "input.rs",
                "lib.rs",
                "pointer.rs",
                "scanout.rs",
                "server.rs",
                "shutdown.rs",
                "startmenu.rs",
                "supervisor.rs",
                "taskbar.rs",
                "uifont.rs",
                "wallpaper.rs",
                "window.rs",
            ][..],
        ),
        (
            "terminal",
            &[
                "atlas.rs",
                "client.rs",
                "configure.rs",
                "ffi.rs",
                "input.rs",
                "lib.rs",
                "model.rs",
                "model/parser.rs",
                "model/reflow.rs",
                "model/screen.rs",
                "model/style.rs",
                "pointer.rs",
                "render.rs",
                "session.rs",
            ][..],
        ),
    ] {
        check_crate(
            root,
            name,
            sources,
            &[
                "crate-type = [\"staticlib\"]",
                "panic = \"abort\"",
                "[dependencies]\ndisplay-proto = { path = \"../display-proto\" }",
            ],
            errors,
        );
    }

    let workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    if !workspace.contains(
        "exclude = [\"bootloader\", \"user/desktop\", \"user/display-proto\", \"user/splash\", \"user/terminal\"]",
    ) {
        errors.push(
            "Cargo.toml: bootloader and the four userspace crates must be the only excluded Rust crates"
                .to_owned(),
        );
    }
    let inittab = fs::read_to_string(root.join("user/base/inittab")).unwrap_or_default();
    let expected_inittab = "::sysinit:/bin/splash\n::respawn:/bin/desktop\n::respawn:/etc/init.d/network-service\n::respawn:-/bin/sh\n";
    if inittab != expected_inittab {
        errors.push(
            "user/base/inittab: must run splash at sysinit and supervise desktop, network and UART recovery exactly once"
                .to_owned(),
        );
    }

    let builder = fs::read_to_string(root.join("scripts/verify_busybox.py")).unwrap_or_default();
    if !builder.contains("def build_desktop(")
        || !builder.contains("def build_terminal(")
        || !builder.contains("def build_splash(")
        || !builder.contains("def display_proto_inputs(")
        || !builder.contains("/bin/desktop")
        || !builder.contains("/bin/terminal")
        || !builder.contains("/bin/splash")
        || !builder.contains("user/desktop/src")
        || !builder.contains("user/terminal/src")
        || !builder.contains("user/splash/src")
        || !builder.contains("user/display-proto")
        || !builder.contains("/etc/terminfo/l/liteos")
        || builder.contains("console-session")
        || [
            "liteui",
            "quickjs",
            "display-session",
            "terminal-service",
            "libseat",
            "libdrm",
        ]
        .iter()
        .any(|marker| builder.contains(marker))
    {
        errors.push(
            "scripts/verify_busybox.py: rootfs must contain only the registered desktop track"
                .to_owned(),
        );
    }
    let atlas = fs::read(root.join("assets/fonts/liteos-terminal.a8")).unwrap_or_default();
    if atlas.get(..8) != Some(b"LTA8\0\0\0\x02") || atlas.len() != 481_136 {
        errors.push(
            "assets/fonts/liteos-terminal.a8: expected the checked v2 terminal atlas".to_owned(),
        );
    }
    let ui_atlas = fs::read(root.join("assets/fonts/liteos-ui.a8p")).unwrap_or_default();
    if ui_atlas.get(..8) != Some(b"LUP8\0\0\0\x01") || ui_atlas.len() != 3_709_860 {
        errors.push(
            "assets/fonts/liteos-ui.a8p: expected the checked v1 UI proportional atlas".to_owned(),
        );
    }
    let wallpaper = fs::read(root.join("assets/wallpaper.xrgb")).unwrap_or_default();
    if wallpaper.get(..8) != Some(b"LWP8\0\0\0\x01") || wallpaper.len() != 6_293_424 {
        errors.push("assets/wallpaper.xrgb: expected the checked v1 raw wallpaper".to_owned());
    }
    let bootlogo = fs::read(root.join("assets/bootlogo.xrgb")).unwrap_or_default();
    if bootlogo.get(..8) != Some(b"LWP8\0\0\0\x01") || bootlogo.len() != 3_145_744 {
        errors.push("assets/bootlogo.xrgb: expected the checked v1 raw boot logo".to_owned());
    }
}
