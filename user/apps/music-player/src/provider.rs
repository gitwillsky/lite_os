//! Provider façade: builds signed requests, performs the HTTPS call, and parses
//! the response into a normalized shape. This is the QQ/NetEase-specific logic
//! that lives inside the `music-player` binary's [`MusicExt`](crate::MusicExt)
//! extension — the generic `lite-runtime` library carries no provider code.
//!
//! Legal note: these providers hit QQ/NetEase's official-but-undocumented
//! endpoints using reverse-engineered signing — fragile (schemes rotate) and
//! outside those services' terms of service. Personal / educational use only.

use serde::Serialize;

use crate::qq::QqSignV1;
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
/// string (the UI loops highest→lowest, one call per tier).
pub(crate) const NETEASE_LEVELS: [&str; 4] = ["hires", "lossless", "exhigh", "standard"];

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
            "qq" => (qq::search_request(query, limit, &QqSignV1), qq::parse_search),
            other => return Err(format!("unknown source '{other}'")),
        };
    let (status, body) = execute(&request)?;
    let parsed = tracks(&body)?;
    let json = serde_json::to_string(&parsed).map_err(|error| error.to_string())?;
    Ok((status, json))
}

/// Resolves a playable URL for a track at one quality tier, returning
/// `(status, {"url": "..."|null} JSON)`. The UI loops highest→lowest quality,
/// so one call resolves exactly one tier. `netease_level` names the tier for
/// NetEase; `qq_quality_index` selects the QQ filename table entry.
pub(crate) fn song_url(
    source: &str,
    id: &str,
    netease_level: &str,
    qq_quality_index: usize,
) -> Result<(u16, String), String> {
    let (status, url) = match source {
        "netease" => {
            let level = if netease_level.is_empty() {
                NETEASE_LEVELS[0]
            } else {
                netease_level
            };
            let (status, body) = execute(&netease::song_url_request(id, level))?;
            (status, netease::parse_song_url(&body)?)
        }
        "qq" => {
            let index = qq_quality_index.min(qq::QUALITY_FILENAMES.len() - 1);
            let (prefix, ext) = qq::QUALITY_FILENAMES[index];
            let filename = qq::build_filename(prefix, id, ext);
            let (status, body) = execute(&qq::song_url_request(id, &filename, &QqSignV1))?;
            (status, qq::parse_song_url(&body)?)
        }
        other => return Err(format!("unknown source '{other}'")),
    };
    let json = serde_json::json!({ "url": url }).to_string();
    Ok((status, json))
}
