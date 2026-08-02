//! QQ Music (y.qq.com) provider.
//!
//! Playable-URL resolution uses the plain, unsigned `musicu.fcg` endpoint
//! with `music.vkey.GetVkey.UrlGetVkey` — the official path that works both
//! anonymously and with a user-supplied login (credential fields in `comm`).
//! The signed `musics.fcg` + `GetEVkey` combination is only needed for the
//! encrypted `mflac`/`mgg` file types, which LiteOS cannot decrypt; it is
//! deliberately unsupported.

use crate::credentials::Credentials;
use crate::provider::{HttpRequest, Method, RemoteTrack, SongUrl, random_hex};

const ENDPOINT: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const MUSIC_DOMAIN: &str = "https://isure.stream.qqmusic.qq.com/";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/134.0.0.0 Safari/537.36";
const URL_VKEY_KEY: &str = "music.vkey.GetVkey.UrlGetVkey";

/// Common params block shared by every musicu request, matching the musicdl
/// reference's `buildcommonparams` (COMMON_DEFAULTS + cv/v/QIMEI36). The
/// QIMEI36 is a device id the mobile methods validate; a real client obtains
/// it via QQ's qimei registration flow (RSA+AES device-fingerprint report,
/// not implemented here). We send a well-formed placeholder — sufficient for
/// many anonymous reads, though device-gated results may still be withheld.
/// `ct` differs per method (search "11", vkey "19" in the reference).
fn common(ct: &str, credentials: Option<&Credentials>) -> serde_json::Value {
    let mut comm = serde_json::json!({
        "ct": ct,
        "cv": "13020508",
        "v": "13020508",
        "QIMEI36": "a3d8f1e2b7c94506e8a1f2d3c4b5a6e7f809",
        "tmeAppID": "qqmusic",
        "format": "json",
        "inCharset": "utf-8",
        "outCharset": "utf-8",
        "uid": "3931641530",
    });
    if let Some(credentials) = credentials {
        if let (Some(uin), Some(key)) = (&credentials.qq_uin, &credentials.qq_music_key) {
            // Reference `buildcommonparams`: qq/authst/tmeLoginType mark the
            // request as logged in; without them VIP vkeys are withheld.
            comm["qq"] = uin.clone().into();
            comm["authst"] = key.clone().into();
            comm["tmeLoginType"] = credentials
                .qq_login_type
                .clone()
                .unwrap_or_else(|| "2".into())
                .into();
        }
    }
    comm
}

/// Builds a POST to the plain musicu endpoint (no `sign` query param).
fn post(request: serde_json::Value) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        url: format!("{ENDPOINT}?format=json"),
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), "https://y.qq.com/".into()),
            ("Origin".into(), "https://y.qq.com".into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: serde_json::to_string(&request).unwrap_or_default(),
    }
}

pub(crate) fn search_request(keyword: &str, limit: u32) -> HttpRequest {
    let param = serde_json::json!({
        "searchid": "60997426243444153",
        "query": keyword,
        "search_type": 0,       // SearchType.SONG
        "num_per_page": limit,
        "page_num": 1,
        "highlight": 0,         // no <em> markup in titles/albums
        "grp": 1,
    });
    post(serde_json::json!({
        "comm": common("11", None),
        "music.search.SearchCgiService.DoSearchForQQMusicMobile": {
            "module": "music.search.SearchCgiService",
            "method": "DoSearchForQQMusicMobile",
            "param": param,
        },
    }))
}

/// Requests a vkey/purl for a song mid. `file_name` encodes the desired
/// quality (e.g. `F000<mid>.flac` for lossless, `M800<mid>.mp3` for 320k).
/// Matches the reference's official plain path: no signature, and no
/// `uin`/`loginflag`/`platform` in param — the login state lives in `comm`.
pub(crate) fn song_url_request(
    song_mid: &str,
    file_name: &str,
    credentials: Option<&Credentials>,
) -> HttpRequest {
    let param = serde_json::json!({
        "guid": random_hex(32),
        "songmid": [song_mid],
        "songtype": [0],
        "filename": [file_name],
    });
    post(serde_json::json!({
        "comm": common("19", credentials),
        URL_VKEY_KEY: { "module": "music.vkey.GetVkey", "method": "UrlGetVkey", "param": param },
    }))
}

/// Filename prefixes to try, highest quality first, in the reference's
/// `SongFileType.SORTED_QUALITIES` order restricted to what the LiteOS
/// decoder plays (flac / vorbis-in-ogg / mp3 / aac-in-m4a). Omitted on
/// purpose: AI00/Q000/Q001 (master/Atmos — SVIP-gated, huge files) and every
/// encrypted mflac/mgg variant (no decryptor).
pub(crate) const QUALITY_FILENAMES: [(&str, &str); 10] = [
    ("F000", "flac"),
    ("O801", "ogg"),
    ("O800", "ogg"),
    ("O600", "ogg"),
    ("O400", "ogg"),
    ("M800", "mp3"),
    ("M500", "mp3"),
    ("C600", "m4a"),
    ("C400", "m4a"),
    ("C200", "m4a"),
];

pub(crate) fn build_filename(prefix: &str, mid: &str, ext: &str) -> String {
    format!("{prefix}{mid}{mid}.{ext}")
}

pub(crate) fn parse_search(body: &str) -> Result<Vec<RemoteTrack>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("qq search json: {error}"))?;
    // musicu.fcg mobile method: songs at
    // <key>.data.body.item_song, keyed by the module.method name.
    let list = value
        .get("music.search.SearchCgiService.DoSearchForQQMusicMobile")
        .and_then(|svc| svc.get("data"))
        .and_then(|data| data.get("body"))
        .and_then(|b| b.get("item_song"))
        .and_then(|songs| songs.as_array())
        .ok_or("qq search: missing item_song")?;
    Ok(list.iter().map(parse_song).collect())
}

fn parse_song(song: &serde_json::Value) -> RemoteTrack {
    let artist = song
        .get("singer")
        .and_then(|s| s.as_array())
        .map(|singers| {
            singers
                .iter()
                .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default();
    let album_mid = song
        .get("album")
        .and_then(|al| al.get("mid"))
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let cover = if album_mid.is_empty() {
        String::new()
    } else {
        format!("https://y.gtimg.cn/music/photo_new/T002R800x800M000{album_mid}.jpg")
    };
    RemoteTrack {
        source: "qq",
        id: song
            .get("mid")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_owned(),
        title: song
            .get("title")
            .or_else(|| song.get("name"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned(),
        artist,
        album: song
            .get("album")
            .and_then(|al| al.get("title").or_else(|| al.get("name")))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned(),
        duration_ms: song
            .get("interval")
            .and_then(|i| i.as_u64())
            .map(|s| s * 1000)
            .unwrap_or(0),
        cover,
        // pay.pay_play == 1 → VIP streaming.
        vip: song
            .get("pay")
            .and_then(|p| p.get("pay_play"))
            .and_then(|v| v.as_i64())
            == Some(1),
    }
}

/// Extracts the playable URL from a UrlGetVkey response. `purl` falls back to
/// `wifiurl` (the reference treats both as playable). Platform-level
/// rejections (e.g. code 104009 `invalidq` when the anonymous device identity
/// fails risk control) fail every quality tier identically, so they surface
/// as a `reason` instead of silently burning through the tier list.
pub(crate) fn parse_song_url(body: &str) -> Result<SongUrl, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("qq url json: {error}"))?;
    if let Some(reason) = platform_error(&value) {
        return Ok(SongUrl::rejected(reason));
    }
    let info = value
        .get(URL_VKEY_KEY)
        .and_then(|svc| svc.get("data"))
        .and_then(|data| data.get("midurlinfo"))
        .and_then(|info| info.get(0));
    let purl = ["purl", "wifiurl"].iter().find_map(|field| {
        info.and_then(|first| first.get(field))
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
    });
    Ok(SongUrl::resolved(purl.map(|p| format!("{MUSIC_DOMAIN}{p}"))))
}

/// Non-zero `code` at the envelope or service level, rendered with the
/// platform's message (`msg`/`retmsg`) when present. A missing code is OK.
fn platform_error(value: &serde_json::Value) -> Option<String> {
    let nodes = [Some(value), value.get(URL_VKEY_KEY)];
    for node in nodes.into_iter().flatten() {
        let code = node.get("code").and_then(|c| c.as_i64());
        if let Some(code) = code.filter(|&code| code != 0) {
            let message = ["msg", "retmsg"]
                .iter()
                .find_map(|field| node.get(field).and_then(|m| m.as_str()))
                .filter(|m| !m.is_empty())
                .unwrap_or("no message");
            return Some(format!("qq vkey rejected: code {code} ({message})"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_block_anonymous_has_no_login_fields() {
        let comm = common("19", None);
        assert_eq!(comm.get("ct").unwrap(), "19");
        assert!(comm.get("qq").is_none());
        assert!(comm.get("authst").is_none());
        assert!(comm.get("tmeLoginType").is_none());
    }

    #[test]
    fn common_block_injects_complete_credentials() {
        let credentials = Credentials {
            qq_uin: Some("123456".into()),
            qq_music_key: Some("KEY".into()),
            qq_login_type: Some("1".into()),
            ..Credentials::default()
        };
        let comm = common("19", Some(&credentials));
        assert_eq!(comm.get("qq").unwrap(), "123456");
        assert_eq!(comm.get("authst").unwrap(), "KEY");
        assert_eq!(comm.get("tmeLoginType").unwrap(), "1");
        // A partial credential (uin without key) must not half-log in.
        let partial = Credentials {
            qq_uin: Some("123456".into()),
            ..Credentials::default()
        };
        assert!(common("19", Some(&partial)).get("qq").is_none());
    }

    #[test]
    fn build_filename_shape() {
        assert_eq!(build_filename("F000", "abc", "flac"), "F000abcabc.flac");
    }

    #[test]
    fn search_request_targets_plain_musicu_endpoint() {
        let request = search_request("周杰伦", 20);
        assert_eq!(request.method, Method::Post);
        assert!(request.url.starts_with("https://u.y.qq.com/cgi-bin/musicu.fcg"));
        assert!(!request.url.contains("sign="));
        assert!(request.body.contains("DoSearchForQQMusicMobile"));
    }

    #[test]
    fn song_url_request_is_unsigned_with_vkey_contract() {
        let request = song_url_request("abc", "F000abcabc.flac", None);
        assert!(request.url.starts_with("https://u.y.qq.com/cgi-bin/musicu.fcg"));
        assert!(!request.url.contains("sign="));
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["comm"]["ct"], "19");
        let param = &body[URL_VKEY_KEY]["param"];
        assert_eq!(param["filename"], serde_json::json!(["F000abcabc.flac"]));
        assert_eq!(param["songmid"], serde_json::json!(["abc"]));
        assert_eq!(param["guid"].as_str().unwrap().len(), 32);
        // The old broken contract sent these; the reference omits them.
        assert!(param.get("uin").is_none());
        assert!(param.get("loginflag").is_none());
        assert!(param.get("platform").is_none());
    }

    #[test]
    fn parse_search_extracts_tracks() {
        let body = r#"{"music.search.SearchCgiService.DoSearchForQQMusicMobile":
            {"data":{"body":{"item_song":[
              {"mid":"abc","title":"T","interval":200,
               "singer":[{"name":"S"}],
               "album":{"mid":"amid","title":"AL"},
               "pay":{"pay_play":1}}
            ]}}}}"#;
        let tracks = parse_search(body).unwrap();
        assert_eq!(tracks.len(), 1);
        let track = &tracks[0];
        assert_eq!(track.source, "qq");
        assert_eq!(track.id, "abc");
        assert_eq!(track.title, "T");
        assert_eq!(track.artist, "S");
        assert_eq!(track.album, "AL");
        assert_eq!(track.duration_ms, 200_000);
        assert!(track.cover.contains("amid"));
        assert!(track.vip);
    }

    #[test]
    fn parse_song_url_joins_domain_and_falls_back_to_wifiurl() {
        let purl = r#"{"code":0,"music.vkey.GetVkey.UrlGetVkey":
            {"code":0,"data":{"midurlinfo":[{"purl":"path/a.mp3"}]}}}"#;
        let result = parse_song_url(purl).unwrap();
        assert_eq!(
            result.url.as_deref(),
            Some("https://isure.stream.qqmusic.qq.com/path/a.mp3")
        );
        assert!(result.reason.is_none());

        let wifi = r#"{"code":0,"music.vkey.GetVkey.UrlGetVkey":
            {"code":0,"data":{"midurlinfo":[{"purl":"","wifiurl":"path/b.mp3"}]}}}"#;
        assert_eq!(
            parse_song_url(wifi).unwrap().url.as_deref(),
            Some("https://isure.stream.qqmusic.qq.com/path/b.mp3")
        );
    }

    #[test]
    fn parse_song_url_empty_purl_is_tier_unavailable_not_error() {
        let body = r#"{"code":0,"music.vkey.GetVkey.UrlGetVkey":
            {"code":0,"data":{"midurlinfo":[{"purl":"","wifiurl":""}]}}}"#;
        let result = parse_song_url(body).unwrap();
        assert_eq!(result.url, None);
        assert_eq!(result.reason, None);
    }

    #[test]
    fn parse_song_url_surfaces_platform_rejection() {
        // The observed anonymous failure: code 104009 with an invalidq msg.
        let body = r#"{"code":104009,"retcode":104009,"msg":"vkey;invalidq;",
            "music.vkey.GetVkey.UrlGetVkey":{"code":104009,"data":{"midurlinfo":[{"purl":""}]}}}"#;
        let result = parse_song_url(body).unwrap();
        assert_eq!(result.url, None);
        let reason = result.reason.unwrap();
        assert!(reason.contains("104009"), "{reason}");
        assert!(reason.contains("invalidq"), "{reason}");
    }
}
