//! `kuro-api` — Kuro Games launcher protocol.
//!
//! Types and client for the same `index.json` / krpdiff API the official
//! launcher uses (verified against the live WuWa CN CDN, 2026-08).

pub mod client;
pub mod config;
pub mod error;
pub mod retry;
pub mod types;

pub use client::{build_client, ApiClient};
pub use config::{
    game_server_by_app_id, index_url, server_entry, servers, Game, Server, ServerEntry, WUWA_PACK_CONFIG_URLS,
};
pub use error::{Error, Result};
pub use types::{
    CdnNode, ChunkInfo, DefaultSection, FileRef, GroupInfo, LauncherConfig, LauncherIndex, LocalConfig, PackBundle,
    PackCdnNode, PackConfig, PackEntry, PatchConfig, PatchIndex, ResourceItem,
};
