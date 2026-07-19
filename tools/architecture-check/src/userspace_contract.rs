use std::{collections::BTreeSet, fs, path::Path};

use super::rust_files;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    let allowed = BTreeSet::from(["README.md", "base", "console-session", "diagnostics"]);
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
            "user/: expected the single TUI track {expected:?}, found {actual:?}"
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

    let console = root.join("user/console-session");
    let expected_crate = BTreeSet::from([
        "Cargo.lock".to_owned(),
        "Cargo.toml".to_owned(),
        "src".to_owned(),
    ]);
    let actual_crate = fs::read_dir(&console)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if actual_crate != expected_crate {
        errors.push(format!(
            "user/console-session: expected exactly {expected_crate:?}, found {actual_crate:?}"
        ));
    }

    let expected_sources = BTreeSet::from([
        "atlas.rs",
        "display.rs",
        "ffi.rs",
        "lib.rs",
        "model.rs",
        "model/parser.rs",
        "model/reflow.rs",
        "model/screen.rs",
        "model/style.rs",
        "reactor.rs",
        "reactor/evdev.rs",
        "reactor/input.rs",
        "reactor/pointer.rs",
        "reactor/session.rs",
    ]);
    let mut source_paths = Vec::new();
    if let Err(error) = rust_files(&console.join("src"), &mut source_paths) {
        errors.push(error);
    }
    let actual_sources = source_paths
        .iter()
        .filter_map(|path| path.strip_prefix(console.join("src")).ok())
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<BTreeSet<_>>();
    let expected_sources = expected_sources
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if actual_sources != expected_sources {
        errors.push(format!(
            "user/console-session/src: expected exactly {expected_sources:?}, found {actual_sources:?}"
        ));
    }

    let manifest = fs::read_to_string(console.join("Cargo.toml")).unwrap_or_default();
    for required in [
        "name = \"console-session\"",
        "crate-type = [\"staticlib\"]",
        "panic = \"abort\"",
    ] {
        if !manifest.contains(required) {
            errors.push(format!(
                "user/console-session/Cargo.toml: missing `{required}`"
            ));
        }
    }
    if manifest.contains("[dependencies]") || manifest.contains(" path = ") {
        errors.push(
            "user/console-session: the unique console Module must remain dependency-free"
                .to_owned(),
        );
    }

    let workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    if !workspace.contains("exclude = [\"bootloader\", \"user/console-session\"]") {
        errors.push(
            "Cargo.toml: bootloader and console-session must be the only excluded Rust crates"
                .to_owned(),
        );
    }
    let inittab = fs::read_to_string(root.join("user/base/inittab")).unwrap_or_default();
    let expected_inittab = "::respawn:/bin/console-session\n::respawn:/etc/init.d/network-service\n::respawn:-/bin/sh\n";
    if inittab != expected_inittab {
        errors.push(
            "user/base/inittab: must supervise console, network and UART recovery exactly once"
                .to_owned(),
        );
    }

    let builder = fs::read_to_string(root.join("scripts/verify_busybox.py")).unwrap_or_default();
    if !builder.contains("def build_console_session(")
        || !builder.contains("/bin/console-session")
        || !builder.contains("user/console-session/src")
        || !builder.contains("/etc/terminfo/l/liteos")
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
            "scripts/verify_busybox.py: rootfs must contain only the registered console session track"
                .to_owned(),
        );
    }
    let atlas = fs::read(root.join("assets/fonts/liteos-terminal.a8")).unwrap_or_default();
    if atlas.get(..8) != Some(b"LTA8\0\0\0\x02") || atlas.len() != 481_136 {
        errors.push(
            "assets/fonts/liteos-terminal.a8: expected the checked v2 terminal atlas".to_owned(),
        );
    }
}
