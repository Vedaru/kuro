//! `kuro-api` — Kuro Games launcher protocol.
//!
//! Types and client for the same `index.json` / krpdiff API the official
//! launcher uses (verified against the live WuWa CN CDN, 2026-08).

pub mod client;
pub mod config;
pub mod error;
pub mod types;

pub use client::ApiClient;
pub use config::{game_server_by_app_id, server_entry, servers, Game, Server, ServerEntry};
pub use error::{Error, Result};
pub use types::{
    ChunkInfo, CdnNode, DefaultSection, FileRef, GroupInfo, LauncherConfig, LauncherIndex,
    LocalConfig, PatchConfig, PatchIndex, PredownloadSection, ResourceItem,
};
