use serde::{Deserialize, Serialize};

pub(super) fn app_metadata(id: &str) -> (&'static str, &'static str) {
    match id {
        "terminal" => ("Terminal", "assets/terminal.png"),
        "my-computer" => ("我的电脑", "assets/computer.png"),
        "file-manager" => ("File Manager", "assets/computer.png"),
        "music-player" => ("Music Player", "assets/speaker.png"),
        _ => ("Application", "assets/terminal.png"),
    }
}

// Installed application bundles. Each `<id>/app.json` is one launchable app;
// `apps.launch` spawns `/bin/lite-ui --app <id>` against `<APPS_ROOT>/<id>`.
const APPS_ROOT: &str = "/usr/share/liteos/apps";
// The desktop bundle ships exactly these icons (see ui/build.mjs). The start
// menu renders under the desktop role, so `src="assets/<name>"` only resolves
// against the desktop root — a manifest icon outside this set cannot load, so
// it is normalized to a shipped name (or the fallback) rather than trusted.
const DESKTOP_ICON_NAMES: [&str; 5] = [
    "computer.png",
    "terminal.png",
    "documents.png",
    "trash.png",
    "speaker.png",
];
pub(super) const FALLBACK_ICON: &str = "assets/terminal.png";

/// On-disk per-app manifest (`<id>/app.json`). Extra fields (entry/style) are
/// ignored here; only what the launcher chrome needs is deserialized.
#[derive(Deserialize)]
struct AppManifest {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    icon: Option<String>,
}

/// The launcher registry entry the desktop React consumes (matches AppMeta in
/// ui/types/lite.d.ts).
#[derive(Serialize)]
struct AppMeta {
    id: String,
    name: String,
    description: String,
    icon: String,
}

/// Constrains a manifest icon to an asset the desktop bundle actually ships,
/// falling back to the terminal icon so the start menu never renders a missing
/// image (mirrors `app_metadata`'s fallback).
pub(super) fn normalize_icon(icon: Option<&str>) -> String {
    let name = icon
        .and_then(|value| value.rsplit('/').next())
        .filter(|name| DESKTOP_ICON_NAMES.contains(name));
    match name {
        Some(name) => format!("assets/{name}"),
        None => FALLBACK_ICON.to_owned(),
    }
}

/// Enumerates `<APPS_ROOT>/*/app.json` into the launcher registry. Unreadable
/// directories, missing/malformed manifests, and ids that fail `valid_app_id`
/// are skipped rather than fatal, so one bad bundle never blanks the menu.
/// Results are sorted by id for a deterministic render order.
pub(super) fn scan_apps() -> String {
    let mut apps: Vec<AppMeta> = match std::fs::read_dir(APPS_ROOT) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let manifest = std::fs::read_to_string(entry.path().join("app.json")).ok()?;
                let manifest: AppManifest = serde_json::from_str(&manifest).ok()?;
                valid_app_id(&manifest.id).then_some(())?;
                Some(AppMeta {
                    icon: normalize_icon(manifest.icon.as_deref()),
                    id: manifest.id,
                    name: manifest.name,
                    description: manifest.description,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    serde_json::to_string(&apps).unwrap_or_else(|_| "[]".to_owned())
}

pub(super) fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_icon_is_constrained_to_shipped_desktop_assets() {
        assert_eq!(
            super::normalize_icon(Some("assets/speaker.png")),
            "assets/speaker.png"
        );
        assert_eq!(
            super::normalize_icon(Some("speaker.png")),
            "assets/speaker.png"
        );
        assert_eq!(
            super::normalize_icon(Some("assets/custom.png")),
            super::FALLBACK_ICON
        );
        assert_eq!(super::normalize_icon(None), super::FALLBACK_ICON);
    }
}
