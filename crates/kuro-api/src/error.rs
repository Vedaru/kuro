//! Error types for kuro-api.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
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

    #[error("unimplemented: {0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
