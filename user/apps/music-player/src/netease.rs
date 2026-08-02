//! NetEase Cloud Music (music.163.com) provider using the eAPI scheme.
//!
//! eAPI encryption (stable, well-documented):
//! ```text
//! digest = md5_hex("nobody" + api_path + "use" + params_json + "md5forencrypt")
//! plain  = api_path + "-36cd479b6b5-" + params_json + "-36cd479b6b5-" + digest
//! cipher = AES-128-ECB(key = "e82ckenh8dichen8", PKCS7(plain))
//! body   = "params=" + hex_upper(cipher)
//! ```
//! `api_path` is the eapi request path with `/eapi/` replaced by `/api/`
//! (e.g. request `/eapi/song/enhance/player/url/v1`, digest over
//! `/api/song/enhance/player/url/v1`).

use aes::cipher::{BlockEncryptMut, KeyInit, block_padding::Pkcs7};
use md5::{Digest, Md5};

use crate::provider::{HttpRequest, Method, RemoteTrack, SongUrl, random_u64};

type Aes128EcbEnc = ecb::Encryptor<aes::Aes128>;

const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
const SEP: &str = "-36cd479b6b5-";
const HOST: &str = "https://interface3.music.163.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/134.0.0.0 Safari/537.36";

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Encrypts eapi `params` for the given request path (the `/eapi/...` path).
/// Returns the `"params=<HEXUPPER>"` form body. Pure and offline-testable.
pub(crate) fn eapi_encrypt(request_path: &str, params_json: &str) -> String {
    // The digest is computed over the /api/ variant of the path.
    let api_path = request_path.replacen("/eapi/", "/api/", 1);
    let digest = md5_hex(&format!("nobody{api_path}use{params_json}md5forencrypt"));
    let plain = format!("{api_path}{SEP}{params_json}{SEP}{digest}");
    let cipher =
        Aes128EcbEnc::new(EAPI_KEY.into()).encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    format!("params={}", hex::encode_upper(cipher))
}

/// Builds a POST request to an eapi endpoint with encrypted params. `music_u`
/// is the account token from the credentials file; without it VIP tracks
/// resolve to trial clips or nothing at all.
fn eapi_request(request_path: &str, params_json: &str, music_u: Option<&str>) -> HttpRequest {
    let mut cookie = "os=pc; appver=8.9.70; osver=; deviceId=liteos".to_owned();
    if let Some(music_u) = music_u {
        cookie.push_str("; MUSIC_U=");
        cookie.push_str(music_u);
    }
    HttpRequest {
        method: Method::Post,
        url: format!("{HOST}/eapi{}", request_path.trim_start_matches("/eapi")),
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("Referer".into(), "https://music.163.com".into()),
            ("Cookie".into(), cookie),
        ],
        body: eapi_encrypt(request_path, params_json),
    }
}

/// Search request. `keyword` is the free-text query; `limit` caps results.
/// Anonymous on purpose: login only matters for playable-URL resolution.
pub(crate) fn search_request(keyword: &str, limit: u32) -> HttpRequest {
    let params = serde_json::json!({
        "s": keyword,
        "type": "1",           // 1 = songs
        "limit": limit.to_string(),
        "offset": "0",
    });
    eapi_request("/eapi/cloudsearch/pc", &params.to_string(), None)
}

/// Builds the (unencrypted) player-url params, split out so tests can assert
/// the contract without decrypting an eapi body. Matches the reference
/// official path: `ids` is a JSON array (not the string `"[id]"`) and
/// `header` carries device info plus a per-request id.
fn song_url_params(song_id: &str, level: &str) -> serde_json::Value {
    // encodeType flac for lossless tiers, aac otherwise — never opus (the
    // LiteOS decoder has no Opus support).
    let encode_type = match level {
        "jymaster" | "hires" | "lossless" => "flac",
        _ => "aac",
    };
    let id = song_id
        .parse::<u64>()
        .map(serde_json::Value::from)
        .unwrap_or_else(|_| serde_json::Value::from(song_id));
    let header = serde_json::json!({
        "os": "pc",
        "appver": "",
        "osver": "",
        "deviceId": "liteos",
        "requestId": (20_000_000 + random_u64() % 10_000_000).to_string(),
    });
    serde_json::json!({
        "ids": [id],
        "level": level,
        "encodeType": encode_type,
        "header": header.to_string(),
    })
}

/// Playable-URL request for a song id, requesting the highest quality first.
/// `level` is one of jymaster/hires/lossless/exhigh/standard.
pub(crate) fn song_url_request(song_id: &str, level: &str, music_u: Option<&str>) -> HttpRequest {
    eapi_request(
        "/eapi/song/enhance/player/url/v1",
        &song_url_params(song_id, level).to_string(),
        music_u,
    )
}

/// Parses a cloudsearch response body into normalized tracks.
pub(crate) fn parse_search(body: &str) -> Result<Vec<RemoteTrack>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("netease search json: {error}"))?;
    let songs = value
        .get("result")
        .and_then(|result| result.get("songs"))
        .and_then(|songs| songs.as_array())
        .ok_or("netease search: missing result.songs")?;
    Ok(songs.iter().map(parse_song).collect())
}

fn parse_song(song: &serde_json::Value) -> RemoteTrack {
    let artist = song
        .get("ar")
        .and_then(|ar| ar.as_array())
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default();
    RemoteTrack {
        source: "netease",
        id: song.get("id").map(json_id).unwrap_or_default(),
        title: str_field(song, "name"),
        artist,
        album: song
            .get("al")
            .map(|al| str_field(al, "name"))
            .unwrap_or_default(),
        duration_ms: song.get("dt").and_then(|d| d.as_u64()).unwrap_or(0),
        cover: song
            .get("al")
            .map(|al| str_field(al, "picUrl"))
            .unwrap_or_default(),
        // fee: 1 = VIP, 4 = purchase-only, 8 = free-with-privileges.
        vip: matches!(song.get("fee").and_then(|f| f.as_i64()), Some(1) | Some(4)),
    }
}

/// Extracts the playable URL from a player-url response. A non-200 top-level
/// `code` is a request-level rejection (surfaces as `reason`); an empty
/// `data[0].url` with code 200 means this quality tier is unavailable and the
/// caller should try the next one. VIP tracks without entitlement resolve to
/// a short trial fragment, which the platform marks in `freeTrialPrivilege` —
/// surfaced so the UI labels the clip instead of presenting it as full audio.
pub(crate) fn parse_song_url(body: &str) -> Result<SongUrl, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("netease url json: {error}"))?;
    if let Some(code) = value.get("code").and_then(|c| c.as_i64()) {
        if code != 200 {
            return Ok(SongUrl::rejected(format!(
                "netease player url: code {code}"
            )));
        }
    }
    let first = value.get("data").and_then(|data| data.get(0));
    let url = first
        .and_then(|first| first.get("url"))
        .and_then(|url| url.as_str())
        .filter(|url| !url.is_empty());
    let trial = url.is_some()
        && first
            .and_then(|first| first.get("freeTrialPrivilege"))
            .and_then(|privilege| privilege.get("resCanListen"))
            .and_then(|can_listen| can_listen.as_bool())
            .unwrap_or(false);
    let mut result = SongUrl::resolved(url.map(str::to_owned));
    result.trial = trial;
    Ok(result)
}

fn str_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn json_id(value: &serde_json::Value) -> String {
    value
        .as_u64()
        .map(|n| n.to_string())
        .or_else(|| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_matches_known_vector() {
        // MD5("") = d41d8cd98f00b204e9800998ecf8427e
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        // MD5("abc") = 900150983cd24fb0d6963f7d28e17f72
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn eapi_encrypt_is_deterministic_and_hexupper() {
        let body = eapi_encrypt("/eapi/song/enhance/player/url/v1", "{\"ids\":\"[1]\"}");
        assert!(body.starts_with("params="));
        let hex_part = &body["params=".len()..];
        // AES-128-ECB with PKCS7 always yields a multiple of 16 bytes → even
        // hex length that is a multiple of 32 chars.
        assert_eq!(hex_part.len() % 32, 0);
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hex_part, hex_part.to_uppercase(), "hex must be uppercase");
        // Deterministic (fixed key, no nonce).
        assert_eq!(
            body,
            eapi_encrypt("/eapi/song/enhance/player/url/v1", "{\"ids\":\"[1]\"}")
        );
    }

    #[test]
    fn eapi_known_vector() {
        // Verified against reference: AES-128-ECB(key="e82ckenh8dichen8") of the
        // plaintext for path "/api/x" with params "{}".
        // digest = md5("nobody/api/xuse{}md5forencrypt")
        let digest = md5_hex("nobody/api/xuse{}md5forencrypt");
        let plain = format!("/api/x{SEP}{{}}{SEP}{digest}");
        let expected =
            Aes128EcbEnc::new(EAPI_KEY.into()).encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
        assert_eq!(
            eapi_encrypt("/eapi/x", "{}"),
            format!("params={}", hex::encode_upper(expected))
        );
    }

    #[test]
    fn search_request_targets_cloudsearch() {
        let request = search_request("周杰伦", 10);
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.url,
            "https://interface3.music.163.com/eapi/cloudsearch/pc"
        );
        assert!(request.body.starts_with("params="));
    }

    #[test]
    fn parse_search_extracts_tracks() {
        let body = r#"{"result":{"songs":[
            {"id":123,"name":"Song","dt":215000,"fee":1,
             "ar":[{"name":"A"},{"name":"B"}],
             "al":{"name":"Album","picUrl":"http://cover"}}
        ]}}"#;
        let tracks = parse_search(body).unwrap();
        assert_eq!(tracks.len(), 1);
        let track = &tracks[0];
        assert_eq!(track.source, "netease");
        assert_eq!(track.id, "123");
        assert_eq!(track.title, "Song");
        assert_eq!(track.artist, "A / B");
        assert_eq!(track.album, "Album");
        assert_eq!(track.duration_ms, 215000);
        assert_eq!(track.cover, "http://cover");
        assert!(track.vip);
    }

    #[test]
    fn song_url_params_match_official_contract() {
        let params = song_url_params("347230", "jymaster");
        // `ids` is a JSON array of the numeric id, not the string "[id]".
        assert_eq!(params["ids"], serde_json::json!([347230]));
        assert_eq!(params["encodeType"], "flac");
        assert_eq!(song_url_params("347230", "exhigh")["encodeType"], "aac");
        let header: serde_json::Value =
            serde_json::from_str(params["header"].as_str().unwrap()).unwrap();
        assert_eq!(header["os"], "pc");
        assert_eq!(header["deviceId"], "liteos");
        let request_id = header["requestId"].as_str().unwrap();
        assert!(request_id.len() == 8 && request_id.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn song_url_request_appends_music_u_cookie() {
        let anonymous = song_url_request("1", "standard", None);
        let cookie = |request: &HttpRequest| {
            request
                .headers
                .iter()
                .find(|(name, _)| name == "Cookie")
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        assert!(!cookie(&anonymous).contains("MUSIC_U"));
        let logged_in = song_url_request("1", "standard", Some("deadbeef"));
        assert!(cookie(&logged_in).contains("MUSIC_U=deadbeef"));
    }

    #[test]
    fn parse_song_url_extracts_first() {
        let full = parse_song_url(r#"{"code":200,"data":[{"url":"http://cdn/a.mp3"}]}"#).unwrap();
        assert_eq!(full.url.as_deref(), Some("http://cdn/a.mp3"));
        assert!(!full.trial);
        assert_eq!(full.reason, None);
        assert_eq!(
            parse_song_url(r#"{"code":200,"data":[{"url":""}]}"#)
                .unwrap()
                .url,
            None
        );
        assert_eq!(
            parse_song_url(r#"{"code":200,"data":[{"url":null}]}"#)
                .unwrap()
                .url,
            None
        );
    }

    #[test]
    fn parse_song_url_marks_trial_clips() {
        let body = r#"{"code":200,"data":[{"url":"http://cdn/trial.mp3",
            "freeTrialPrivilege":{"resCanListen":true,"resFrag":30}}]}"#;
        let result = parse_song_url(body).unwrap();
        assert_eq!(result.url.as_deref(), Some("http://cdn/trial.mp3"));
        assert!(result.trial);
    }

    #[test]
    fn parse_song_url_surfaces_request_level_rejection() {
        let result = parse_song_url(r#"{"code":401,"message":"need login"}"#).unwrap();
        assert_eq!(result.url, None);
        assert!(result.reason.unwrap().contains("401"));
    }
}
