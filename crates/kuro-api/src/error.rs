//! Error types for kuro-api.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {}", http_chain(.0))]
    Http(#[from] reqwest::Error),

    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("no usable CDN node in cdnList")]
    NoCdnNode,

    #[error("missing required field `{0}`")]
    MissingField(&'static str),

    #[error("local launcher config not found at {0}; run with --path or set up the game first")]
    NoLocalConfig(PathBuf),

    #[error("unknown appId `{0}` — cannot map to a known game/server")]
    UnknownAppId(String),

    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("patch error: {0}")]
    Patch(String),

    #[error("manifest path `{0}` is unsafe (absolute, drive-qualified or escaping the install root)")]
    UnsafePath(String),

    #[error("{0}")]
    TokenMissing(String),

    #[error("unimplemented: {0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Flatten a `reqwest::Error`'s `source()` chain into the message.
///
/// `reqwest::Error`'s own `Display` is often just `error sending request for
/// url (…)`; the reason (DNS failure, connect timeout, TLS alert, body error)
/// is one or two `source()` hops down. Without this the report is
/// unactionable, which is what made a real CDN stall look like a mystery.
fn http_chain(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}
