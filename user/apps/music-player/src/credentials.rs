//! Optional user-provided login state for the QQ/NetEase providers.
//!
//! VIP tracks require an authenticated account: anonymous requests only
//! receive trial clips or empty URLs. The user supplies their own account by
//! pasting browser cookies into [`CREDENTIALS_PATH`] inside the guest
//! filesystem:
//! ```json
//! {
//!   "netease": { "cookie": "MUSIC_U=...; ..." },
//!   "qq": { "cookie": "uin=o123456; qqmusic_key=...; tmeLoginType=2; ..." }
//! }
//! ```
//! The file is re-read on every song-url resolution: it is a few hundred
//! bytes, and editing it must take effect without restarting the app, so
//! there is deliberately no cache. A missing or malformed file degrades
//! silently to anonymous access.

use std::collections::HashMap;

/// Path (in the guest filesystem) of the optional credentials file.
pub(crate) const CREDENTIALS_PATH: &str = "/root/music-credentials.json";

/// Login state extracted from the credentials file. Every field is optional;
/// absent fields mean the corresponding provider stays anonymous.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    /// NetEase `MUSIC_U` cookie (the account token).
    pub netease_music_u: Option<String>,
    /// QQ uin (QQ number, the cookie's leading `o` stripped).
    pub qq_uin: Option<String>,
    /// QQ `qqmusic_key` (or legacy `musickey`) cookie.
    pub qq_music_key: Option<String>,
    /// QQ `tmeLoginType`; derived from the key shape when absent.
    pub qq_login_type: Option<String>,
}

/// Reads and parses [`CREDENTIALS_PATH`]; any failure yields anonymous.
pub(crate) fn load() -> Credentials {
    load_from(CREDENTIALS_PATH)
}

fn load_from(path: &str) -> Credentials {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Credentials::default();
    };
    parse(&text)
}

fn parse(text: &str) -> Credentials {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Credentials::default();
    };
    let cookie_of = |source: &str| -> HashMap<String, String> {
        value
            .get(source)
            .and_then(|section| section.get("cookie"))
            .and_then(|cookie| cookie.as_str())
            .map(parse_cookie)
            .unwrap_or_default()
    };
    let netease = cookie_of("netease");
    let qq = cookie_of("qq");
    let music_key = qq
        .get("qqmusic_key")
        .or_else(|| qq.get("musickey"))
        .filter(|key| !key.is_empty())
        .cloned();
    let login_type = qq.get("tmeLoginType").cloned().or_else(|| {
        // The reference rule (musicdl Credential): WeChat-session keys carry
        // a "W_X" prefix and log in as type 1, QQ sessions as type 2.
        music_key
            .as_deref()
            .map(|key| if key.starts_with("W_X") { "1" } else { "2" }.to_owned())
    });
    Credentials {
        netease_music_u: netease.get("MUSIC_U").filter(|u| !u.is_empty()).cloned(),
        qq_uin: qq
            .get("uin")
            .map(|uin| uin.trim_start_matches('o').to_owned())
            .filter(|uin| !uin.is_empty()),
        qq_music_key: music_key,
        qq_login_type: login_type,
    }
}

/// Splits a `k=v; k2=v2` cookie header into a map (last occurrence wins).
fn parse_cookie(cookie: &str) -> HashMap<String, String> {
    cookie
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cookie_splits_pairs() {
        let map = parse_cookie("uin=o123456; qqmusic_key=ABC; tmeLoginType=2");
        assert_eq!(map.get("uin").unwrap(), "o123456");
        assert_eq!(map.get("qqmusic_key").unwrap(), "ABC");
        assert_eq!(map.get("tmeLoginType").unwrap(), "2");
    }

    #[test]
    fn parse_full_file_extracts_both_sources() {
        let text = r#"{
            "netease": { "cookie": "MUSIC_U=deadbeef; other=1" },
            "qq": { "cookie": "uin=o123456; qqmusic_key=KEY; tmeLoginType=1" }
        }"#;
        let credentials = parse(text);
        assert_eq!(credentials.netease_music_u.as_deref(), Some("deadbeef"));
        assert_eq!(credentials.qq_uin.as_deref(), Some("123456"));
        assert_eq!(credentials.qq_music_key.as_deref(), Some("KEY"));
        assert_eq!(credentials.qq_login_type.as_deref(), Some("1"));
    }

    #[test]
    fn login_type_defaults_from_key_shape() {
        let wechat = parse(r#"{"qq":{"cookie":"uin=o1; qqmusic_key=W_X_abc"}}"#);
        assert_eq!(wechat.qq_login_type.as_deref(), Some("1"));
        let qq_session = parse(r#"{"qq":{"cookie":"uin=o1; qqmusic_key=abc"}}"#);
        assert_eq!(qq_session.qq_login_type.as_deref(), Some("2"));
    }

    #[test]
    fn malformed_or_missing_fields_degrade_to_anonymous() {
        assert_eq!(parse("not json"), Credentials::default());
        assert_eq!(parse("{}"), Credentials::default());
        assert_eq!(
            parse(r#"{"qq":{"cookie":"tmeLoginType=2"}}"#),
            Credentials {
                qq_login_type: Some("2".into()),
                ..Credentials::default()
            }
        );
    }

    #[test]
    fn missing_file_is_anonymous() {
        assert_eq!(
            load_from("/nonexistent/music-credentials.json"),
            Credentials::default()
        );
    }
}
