//! The desktop launcher's application registry.
//!
//! Scans `<apps_root>/*/app.json` into the launcher card list the desktop React
//! consumes, validates app ids, and constrains manifest icons to the assets the
//! desktop bundle actually ships. This is desktop *policy* — it lives in the
//! desktop binary, not the generic runtime library.

use std::path::Path;

use serde::{Deserialize, Serialize};

// The desktop bundle ships exactly these icons (see ui/build.mjs). Launcher
// cards render under the desktop role, so `src="assets/<name>"` resolves only
// against the desktop root; manifest icons are constrained to that set.
const DESKTOP_ICON_NAMES: [&str; 6] = [
    "files.png",
    "terminal.png",
    "music.png",
    "package.png",
    "settings.png",
    "liteos.png",
];
const FALLBACK_ICON: &str = "assets/terminal.png";

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
/// falling back to the terminal icon so the launcher never renders a missing
/// image.
fn normalize_icon(icon: Option<&str>) -> String {
    let name = icon
        .and_then(|value| value.rsplit('/').next())
        .filter(|name| DESKTOP_ICON_NAMES.contains(name));
    match name {
        Some(name) => format!("assets/{name}"),
        None => FALLBACK_ICON.to_owned(),
    }
}

/// Enumerates `<apps_root>/*/app.json` into the launcher registry. Unreadable
/// directories, missing/malformed manifests, and ids that fail `valid_app_id`
/// are skipped rather than fatal, so one bad bundle never blanks the menu.
/// Results are sorted by id for a deterministic render order.
pub(crate) fn scan_apps(apps_root: &Path) -> String {
    let mut apps: Vec<AppMeta> = match std::fs::read_dir(apps_root) {
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

pub(crate) fn valid_app_id(id: &str) -> bool {
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
            super::normalize_icon(Some("assets/music.png")),
            "assets/music.png"
        );
        assert_eq!(super::normalize_icon(Some("music.png")), "assets/music.png");
        assert_eq!(
            super::normalize_icon(Some("assets/custom.png")),
            super::FALLBACK_ICON
        );
        assert_eq!(super::normalize_icon(None), super::FALLBACK_ICON);
    }

    #[test]
    fn valid_app_id_accepts_kebab_and_rejects_bad() {
        assert!(super::valid_app_id("music-player"));
        assert!(super::valid_app_id("file-manager"));
        assert!(!super::valid_app_id(""));
        assert!(!super::valid_app_id("Bad_Id"));
        assert!(!super::valid_app_id("../etc"));
    }
}
