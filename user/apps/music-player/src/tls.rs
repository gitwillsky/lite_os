//! Shared blocking HTTP client (ureq + rustls + bundled webpki roots).
//!
//! The guest ships no CA bundle, so trust roots are compiled in via
//! `webpki-roots`. One `Agent` is built once and shared across the service's
//! connection threads.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::{ClientConfig, RootCertStore};

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// Returns the process-wide rustls-backed ureq agent, building it on first use.
pub(crate) fn agent() -> ureq::Agent {
    AGENT.get_or_init(build).clone()
}

fn build() -> ureq::Agent {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    ureq::AgentBuilder::new()
        .tls_config(Arc::new(config))
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36",
        )
        .build()
}
