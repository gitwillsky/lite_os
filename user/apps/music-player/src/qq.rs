//! QQ Music (y.qq.com) provider.
//!
//! WARNING: the `musics.fcg` request `sign` is a reverse-engineered, obfuscated
//! scheme that Tencent rotates without notice. It is deliberately isolated
//! behind [`QqSigner`] so that a rotation is a one-function fix (`QqSignV1`),
//! not a scattered change.
//!
//! Current sign (SHA-1 based):
//! ```text
//! hash   = SHA1(compact_json(request)).hex().upper()          // 40 hex chars
//! part1  = hash chars at [23,14,6,36,16,40*,7,19]  (*>=40 dropped)
//! part2  = hash chars at [16,1,32,12,19,27,8,5]
//! part3  = 20 bytes: SCRAMBLE[i] ^ u8::from_str_radix(hash[2i..2i+2], 16)
//! mid    = base64(part3) with \ / + = stripped
//! sign   = ("zzc" + part1 + mid + part2).to_lowercase()
//! ```

use base64::Engine as _;
use sha1::{Digest, Sha1};

use crate::provider::{HttpRequest, Method, RemoteTrack};

const ENC_ENDPOINT: &str = "https://u.y.qq.com/cgi-bin/musics.fcg";
const ENDPOINT: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const MUSIC_DOMAIN: &str = "https://isure.stream.qqmusic.qq.com/";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/134.0.0.0 Safari/537.36";

/// Isolates the fragile QQ signing scheme so rotations touch one impl only.
pub(crate) trait QqSigner {
    fn sign(&self, compact_request_json: &str) -> String;
}

/// The signing scheme current as of implementation.
pub(crate) struct QqSignV1;

impl QqSigner for QqSignV1 {
    fn sign(&self, compact_request_json: &str) -> String {
        const PART_1_INDEXES: [usize; 8] = [23, 14, 6, 36, 16, 40, 7, 19];
        const PART_2_INDEXES: [usize; 8] = [16, 1, 32, 12, 19, 27, 8, 5];
        const SCRAMBLE: [u8; 20] = [
            89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161,
            121, 179,
        ];

        let mut hasher = Sha1::new();
        hasher.update(compact_request_json.as_bytes());
        let hash = hex::encode_upper(hasher.finalize());
        let hash_bytes = hash.as_bytes();

        let pick = |indexes: &[usize]| -> String {
            indexes
                .iter()
                .filter(|&&i| i < hash.len())
                .map(|&i| hash_bytes[i] as char)
                .collect()
        };
        let part1 = pick(&PART_1_INDEXES);
        let part2 = pick(&PART_2_INDEXES);

        let mut part3 = [0u8; 20];
        for (i, scramble) in SCRAMBLE.iter().enumerate() {
            let byte = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap_or(0);
            part3[i] = scramble ^ byte;
        }
        let mid: String = base64::engine::general_purpose::STANDARD
            .encode(part3)
            .chars()
            .filter(|c| !matches!(c, '\\' | '/' | '+' | '='))
            .collect();

        format!("zzc{part1}{mid}{part2}").to_lowercase()
    }
}

/// Common params block shared by every musicu request, matching the musicdl
/// reference's `buildcommonparams` (COMMON_DEFAULTS + cv/v/QIMEI36). The QIMEI36
/// is a device id the mobile methods validate; a real client generates it via
/// QQ's qimei registration flow. We send a well-formed placeholder — sufficient
/// for many anonymous reads, though device-gated results may still be withheld.
fn common() -> serde_json::Value {
    serde_json::json!({
        "ct": "11",
        "cv": "13020508",
        "v": "13020508",
        "QIMEI36": "a3d8f1e2b7c94506e8a1f2d3c4b5a6e7f809",
        "tmeAppID": "qqmusic",
        "format": "json",
        "inCharset": "utf-8",
        "outCharset": "utf-8",
        "uid": "3931641530",
    })
}

/// Builds the signed request (URL with `sign` query param + JSON body).
fn signed_request(module: &str, method: &str, param: serde_json::Value, signer: &dyn QqSigner) -> HttpRequest {
    let key = format!("{module}.{method}");
    let request = serde_json::json!({
        "comm": common(),
        key: { "module": module, "method": method, "param": param },
    });
    // serde_json is compact by default (no spaces) — matches orjson.dumps.
    let body = serde_json::to_string(&request).unwrap_or_default();
    let sign = signer.sign(&body);
    HttpRequest {
        method: Method::Post,
        url: format!("{ENC_ENDPOINT}?sign={sign}&format=json"),
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), "https://y.qq.com/".into()),
            ("Origin".into(), "https://y.qq.com".into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body,
    }
}

pub(crate) fn search_request(keyword: &str, limit: u32, _signer: &dyn QqSigner) -> HttpRequest {
    // The musicdl reference defaults to the PLAIN unsigned endpoint
    // (musicu.fcg), not the signed musics.fcg — the plain endpoint returns
    // results anonymously. `_signer` is unused here but kept in the signature
    // so switching to the signed variant is a one-line change.
    let param = serde_json::json!({
        "searchid": "60997426243444153",
        "query": keyword,
        "search_type": 0,       // SearchType.SONG
        "num_per_page": limit,
        "page_num": 1,
        "highlight": 0,         // no <em> markup in titles/albums
        "grp": 1,
    });
    let request = serde_json::json!({
        "comm": common(),
        "music.search.SearchCgiService.DoSearchForQQMusicMobile": {
            "module": "music.search.SearchCgiService",
            "method": "DoSearchForQQMusicMobile",
            "param": param,
        },
    });
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

/// Requests a vkey/purl for a song mid. `file_name` encodes the desired quality
/// (e.g. `F000<mid>.flac` for lossless, `M800<mid>.mp3` for 320k).
pub(crate) fn song_url_request(song_mid: &str, file_name: &str, signer: &dyn QqSigner) -> HttpRequest {
    let param = serde_json::json!({
        "guid": "1234567890",
        "songmid": [song_mid],
        "songtype": [0],
        "uin": "0",
        "loginflag": 1,
        "platform": "20",
        "filename": [file_name],
    });
    signed_request("music.vkey.GetVkey", "UrlGetVkey", param, signer)
}

/// The highest-quality filename prefixes to try, in order. Each entry is
/// (prefix, extension) — F000=flac, M800=320k mp3, M500=128k mp3.
pub(crate) const QUALITY_FILENAMES: [(&str, &str); 3] =
    [("F000", "flac"), ("M800", "mp3"), ("M500", "mp3")];

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

/// Extracts the first non-empty purl and joins it with the CDN domain.
pub(crate) fn parse_song_url(body: &str) -> Result<Option<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("qq url json: {error}"))?;
    let purl = value
        .get("music.vkey.GetVkey.UrlGetVkey")
        .and_then(|svc| svc.get("data"))
        .and_then(|data| data.get("midurlinfo"))
        .and_then(|info| info.get(0))
        .and_then(|first| first.get("purl"))
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty());
    Ok(purl.map(|p| format!("{MUSIC_DOMAIN}{p}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_lowercase_and_prefixed() {
        let sign = QqSignV1.sign(r#"{"comm":{}}"#);
        assert!(sign.starts_with("zzc"));
        assert_eq!(sign, sign.to_lowercase());
        // zzc + 8 (part1) + up to 27 (base64 of 20 bytes, minus stripped) + 8 (part2)
        assert!(sign.len() > "zzc".len() + 16);
    }

    #[test]
    fn sign_is_deterministic() {
        let a = QqSignV1.sign(r#"{"query":"abc"}"#);
        let b = QqSignV1.sign(r#"{"query":"abc"}"#);
        assert_eq!(a, b);
        // Different input → different sign.
        assert_ne!(a, QqSignV1.sign(r#"{"query":"abd"}"#));
    }

    #[test]
    fn sign_known_vector() {
        // SHA1("test") = a94a8fe5ccb19ba61c4c0873d391e987982fbbd3 → upper.
        // Recompute expectation via the same primitives to lock the recipe.
        let hash = "A94A8FE5CCB19BA61C4C0873D391E987982FBBD3";
        assert_eq!(hash.len(), 40);
        let part1: String = [23usize, 14, 6, 36, 16, 7, 19] // 40 dropped (>=40)
            .iter()
            .map(|&i| hash.as_bytes()[i] as char)
            .collect();
        let sign = QqSignV1.sign("test");
        assert!(sign.contains(&part1.to_lowercase()));
    }

    #[test]
    fn build_filename_shape() {
        assert_eq!(build_filename("F000", "abc", "flac"), "F000abcabc.flac");
    }

    #[test]
    fn search_request_targets_plain_musicu_endpoint() {
        let request = search_request("周杰伦", 20, &QqSignV1);
        assert_eq!(request.method, Method::Post);
        assert!(request.url.starts_with("https://u.y.qq.com/cgi-bin/musicu.fcg"));
        // Plain endpoint is unsigned (no sign query param).
        assert!(!request.url.contains("sign="));
        assert!(request.body.contains("DoSearchForQQMusicMobile"));
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
    fn parse_song_url_joins_domain() {
        let body = r#"{"music.vkey.GetVkey.UrlGetVkey":
            {"data":{"midurlinfo":[{"purl":"path/a.mp3"}]}}}"#;
        assert_eq!(
            parse_song_url(body).unwrap(),
            Some("https://isure.stream.qqmusic.qq.com/path/a.mp3".to_owned())
        );
        let empty = r#"{"music.vkey.GetVkey.UrlGetVkey":
            {"data":{"midurlinfo":[{"purl":""}]}}}"#;
        assert_eq!(parse_song_url(empty).unwrap(), None);
    }
}
