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
    let expected_entries = BTreeSet::from(["Cargo.toml".to_owned(), "src".to_owned()]);
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
        "Cargo.lock",
        "Cargo.toml",
        "base",
        "desktop",
        "diagnostics",
        "display-proto",
        "linux-uapi",
        "splash",
        "terminal",
    ]);
    let actual = match fs::read_dir(root.join("user")) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.file_name() != "target")
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

    let user_workspace = fs::read_to_string(root.join("user/Cargo.toml")).unwrap_or_default();
    for required in [
        "members = [\"desktop\", \"display-proto\", \"linux-uapi\", \"splash\", \"terminal\"]",
        "display-proto = { path = \"display-proto\" }",
        "linux-uapi = { path = \"linux-uapi\" }",
        "[profile.release]\npanic = \"abort\"",
    ] {
        if !user_workspace.contains(required) {
            errors.push(format!("user/Cargo.toml: missing `{required}`"));
        }
    }

    // 所有产品应用均为普通 std binary；Linux 缺失接口只允许由 linux-uapi 提供。
    check_crate(
        root,
        "splash",
        &["lib.rs", "render.rs"],
        &[
            "name = \"splash\"",
            "[[bin]]\nname = \"splash\"",
            "linux-uapi.workspace = true",
        ],
        errors,
    );

    check_crate(
        root,
        "display-proto",
        &["lib.rs", "message.rs", "transport.rs"],
        &["name = \"display-proto\"", "linux-uapi.workspace = true"],
        errors,
    );
    check_crate(
        root,
        "linux-uapi",
        &[
            "drm.rs",
            "input.rs",
            "lib.rs",
            "process.rs",
            "pty.rs",
            "raw.rs",
            "unix.rs",
        ],
        &["name = \"linux-uapi\""],
        errors,
    );
    for (name, sources) in [
        (
            "desktop",
            &[
                "chrome.rs",
                "clients.rs",
                "compositor.rs",
                "cursor.rs",
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
        let binary_manifest = format!("[[bin]]\nname = \"{name}\"");
        check_crate(
            root,
            name,
            sources,
            &[
                binary_manifest.as_str(),
                "display-proto.workspace = true",
                "linux-uapi.workspace = true",
            ],
            errors,
        );
    }

    let mut product_sources = Vec::new();
    if let Err(error) = rust_files(&root.join("user"), &mut product_sources) {
        errors.push(error);
    }
    for source in product_sources {
        if source.starts_with(root.join("user/linux-uapi")) {
            continue;
        }
        let text = fs::read_to_string(&source).unwrap_or_default();
        if text.contains("extern \"C\"") || text.contains("#[link(") {
            errors.push(format!(
                "{}: raw FFI is owned exclusively by user/linux-uapi",
                source.display()
            ));
        }
    }

    let workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    for excluded in [
        "\"bootloader\"",
        "\"user/desktop\"",
        "\"user/display-proto\"",
        "\"user/linux-uapi\"",
        "\"user/splash\"",
        "\"user/terminal\"",
    ] {
        if !workspace.contains(excluded) {
            errors.push(format!(
                "Cargo.toml: workspace exclude is missing {excluded}"
            ));
        }
    }
    if workspace.matches("\"user/").count() != 5 {
        errors.push(
            "Cargo.toml: bootloader and the five userspace crates must be the only excluded Rust crates"
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
        || !builder.contains("build-std=std,panic_abort")
        || !builder.contains("--bin")
        || !builder.contains("/bin/desktop")
        || !builder.contains("/bin/terminal")
        || !builder.contains("/bin/splash")
        || !builder.contains("ROOT / \"user/Cargo.toml\"")
        || !builder.contains("ROOT / \"user/Cargo.lock\"")
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
    if ui_atlas.get(..8) != Some(b"LUP8\0\0\0\x01") || ui_atlas.len() != 7_458_232 {
        errors.push(
            "assets/fonts/liteos-ui.a8p: expected the checked v1 UI proportional atlas".to_owned(),
        );
    }
    let wallpaper = fs::read(root.join("assets/wallpaper.xrgb")).unwrap_or_default();
    if wallpaper.get(..8) != Some(b"LWP8\0\0\0\x01") || wallpaper.len() != 20_358_160 {
        errors.push("assets/wallpaper.xrgb: expected the checked v1 raw wallpaper".to_owned());
    }
    let bootlogo = fs::read(root.join("assets/bootlogo.xrgb")).unwrap_or_default();
    if bootlogo.get(..8) != Some(b"LWP8\0\0\0\x01") || bootlogo.len() != 3_145_744 {
        errors.push("assets/bootlogo.xrgb: expected the checked v1 raw boot logo".to_owned());
    }
    let cursor = fs::read(root.join("assets/cursor.lc1")).unwrap_or_default();
    if cursor.get(..8) != Some(b"LCR1\0\0\0\x01") || cursor.len() != 272 {
        errors.push("assets/cursor.lc1: expected the checked v1 32x32 arrow cursor".to_owned());
    }
    let sprites = fs::read(root.join("assets/desktop-sprites.argb")).unwrap_or_default();
    if sprites.get(..8) != Some(b"LSP8\0\0\0\x01") || sprites.len() != 331_792 {
        errors.push(
            "assets/desktop-sprites.argb: expected the checked v1 576x144 sprite sheet".to_owned(),
        );
    }
}
