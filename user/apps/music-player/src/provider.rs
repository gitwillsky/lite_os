//! Provider façade: builds provider requests, performs the HTTPS call, and
//! parses the response into a normalized shape. This is the QQ/NetEase-specific
//! logic that lives inside the `music-player` binary's [`MusicExt`](crate::MusicExt)
//! extension — the generic `lite-runtime` library carries no provider code.
//!
//! Legal note: these providers hit QQ/NetEase's official-but-undocumented
//! endpoints using reverse-engineered request schemes — fragile (they rotate)
//! and outside those services' terms of service. Personal / educational use
//! only; VIP content requires the user's own account (see `credentials.rs`).

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::credentials::Credentials;
use crate::{netease, qq, tls};

/// A fully-formed outbound HTTP request built by a provider.
#[derive(Debug, Clone)]
pub(crate) struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Form/JSON body for POST requests; empty for GET.
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    // `Get` is a legitimate HTTP method the provider layer may build even though
    // the current QQ/NetEase endpoints are all POST.
    #[allow(dead_code)]
    Get,
    Post,
}

/// A normalized search result the UI renders regardless of source.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteTrack {
    /// "qq" | "netease"
    pub source: &'static str,
    /// Stable per-source identifier used to resolve a playable URL.
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    /// Cover art URL (may be empty).
    pub cover: String,
    /// True when the track is likely VIP/paid — playback may resolve to a trial
    /// clip or fail without login cookies.
    pub vip: bool,
}

/// NetEase quality tiers, highest first, indexed by the JS-supplied `level`
/// string (the UI loops highest→lowest, one call per tier). The reference
/// also offers jyeffect/sky/dolby, but those are surround-effect encodings
/// the LiteOS decoder cannot play, so they are omitted.
pub(crate) const NETEASE_LEVELS: [&str; 5] =
    ["jymaster", "hires", "lossless", "exhigh", "standard"];

/// The outcome of resolving one quality tier, before UI interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SongUrl {
    /// Playable URL; `None` when this tier is unavailable (try the next one).
    pub url: Option<String>,
    /// True when the URL is a short trial clip, not the full track.
    pub trial: bool,
    /// Platform/network rejection; `Some` means every tier fails identically
    /// and the UI must stop the downgrade loop and show this reason.
    pub reason: Option<String>,
}

impl SongUrl {
    pub(crate) fn resolved(url: Option<String>) -> Self {
        Self {
            url,
            trial: false,
            reason: None,
        }
    }

    pub(crate) fn rejected(reason: String) -> Self {
        Self {
            url: None,
            trial: false,
            reason: Some(reason),
        }
    }
}

/// Serializes a [`SongUrl`] as the `music.songUrl` reply body.
fn song_url_json(result: &SongUrl) -> String {
    serde_json::json!({
        "url": result.url,
        "kind": if result.trial { "trial" } else { "full" },
        "reason": result.reason,
    })
    .to_string()
}

static RANDOM_SEED: OnceLock<u64> = OnceLock::new();
static RANDOM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Pseudo-random u64 for request `guid`/`requestId` fields. QQ/NetEase expect
/// a fresh-looking id per request but never validate randomness quality, so a
/// splitmix64 stream seeded once from the clock suffices; a fixed value risks
/// server-side dedup of identical requests.
pub(crate) fn random_u64() -> u64 {
    let seed = *RANDOM_SEED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
    });
    let n = RANDOM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = seed.wrapping_add(n.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// `len` lowercase hex chars drawn from [`random_u64`].
pub(crate) fn random_hex(len: usize) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        out.push_str(&format!("{:016x}", random_u64()));
    }
    out.truncate(len);
    out
}

/// Executes an [`HttpRequest`] and returns `(status, body)`.
fn execute(request: &HttpRequest) -> Result<(u16, String), String> {
    let agent = tls::agent();
    let mut builder = match request.method {
        Method::Get => agent.get(&request.url),
        Method::Post => agent.post(&request.url),
    };
    for (name, value) in &request.headers {
        builder = builder.set(name, value);
    }
    let response = match request.method {
        Method::Get => builder.call(),
        Method::Post => builder.send_string(&request.body),
    };
    match response {
        Ok(response) | Err(ureq::Error::Status(_, response)) => {
            let status = response.status();
            response
                .into_string()
                .map(|body| (status, body))
                .map_err(|error| format!("read body: {error}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Searches one source and returns `(status, normalized RemoteTrack[] JSON)`.
pub(crate) fn search(source: &str, query: &str, limit: u32) -> Result<(u16, String), String> {
    let (request, tracks): (HttpRequest, fn(&str) -> Result<Vec<RemoteTrack>, String>) =
        match source {
            "netease" => (netease::search_request(query, limit), netease::parse_search),
            "qq" => (qq::search_request(query, limit), qq::parse_search),
            other => return Err(format!("unknown source '{other}'")),
        };
    let (status, body) = execute(&request)?;
    let parsed = tracks(&body)?;
    let json = serde_json::to_string(&parsed).map_err(|error| error.to_string())?;
    Ok((status, json))
}

/// Resolves a playable URL for a track at one quality tier, returning
/// `(status, song-url JSON)` — see [`song_url_json`] for the body shape. The
/// UI loops highest→lowest quality, so one call resolves exactly one tier.
/// `netease_level` names the tier for NetEase; `qq_quality_index` selects the
/// QQ filename table entry. `credentials` carries the optional user login
/// state injected into the provider requests.
pub(crate) fn song_url(
    source: &str,
    id: &str,
    netease_level: &str,
    qq_quality_index: usize,
    credentials: &Credentials,
) -> Result<(u16, String), String> {
    let (status, result) = match source {
        "netease" => {
            let level = if netease_level.is_empty() {
                NETEASE_LEVELS[0]
            } else {
                netease_level
            };
            let (status, body) = execute(&netease::song_url_request(
                id,
                level,
                credentials.netease_music_u.as_deref(),
            ))?;
            (status, netease::parse_song_url(&body)?)
        }
        "qq" => {
            let index = qq_quality_index.min(qq::QUALITY_FILENAMES.len() - 1);
            let (prefix, ext) = qq::QUALITY_FILENAMES[index];
            let filename = qq::build_filename(prefix, id, ext);
            let (status, body) =
                execute(&qq::song_url_request(id, &filename, Some(credentials)))?;
            (status, qq::parse_song_url(&body)?)
        }
        other => return Err(format!("unknown source '{other}'")),
    };
    Ok((status, song_url_json(&result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_url_json_shape() {
        let full: serde_json::Value =
            serde_json::from_str(&song_url_json(&SongUrl::resolved(Some("http://a".into()))))
                .unwrap();
        assert_eq!(full["url"], "http://a");
        assert_eq!(full["kind"], "full");
        assert!(full["reason"].is_null());

        let trial = SongUrl {
            url: Some("http://t".into()),
            trial: true,
            reason: None,
        };
        let trial: serde_json::Value = serde_json::from_str(&song_url_json(&trial)).unwrap();
        assert_eq!(trial["kind"], "trial");

        let rejected: serde_json::Value =
            serde_json::from_str(&song_url_json(&SongUrl::rejected("denied".into()))).unwrap();
        assert!(rejected["url"].is_null());
        assert_eq!(rejected["reason"], "denied");
    }

    #[test]
    fn random_hex_shape_and_variety() {
        let hex = random_hex(32);
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_hex(32), random_hex(32));
    }
}
