//! Serde types for the Kuro Games launcher protocol.
//!
//! Modeled on the live wire format (verified against the CN WuWa CDN, 2026-08):
//!
//! * `GET {api_url}`  -> `LauncherIndex` (per-game/per-server entry point)
//! * `GET {cdn}/{indexFile}` -> `PatchIndex` (one per source version; lists the
//!   krpdiff groups needed to reach the target version)
//!
//! All extra/mystery fields are ignored (`serde` default behavior) so Kuro can
//! add fields without breaking us.

use serde::{Deserialize, Serialize};

/// Top level of the launcher API entry point (e.g.
/// `.../launcher/game/G152/10003_<token>/index.json`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherIndex {
    pub default: DefaultSection,
    /// Present only while a predownload window is open.
    #[serde(default)]
    pub predownload: Option<PredownloadSection>,
    #[serde(default)]
    pub predownload_switch: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultSection {
    /// Weighted CDN nodes (`P` = weight, `url` = base).
    #[serde(default)]
    pub cdn_list: Vec<CdnNode>,
    pub config: LauncherConfig,
    #[serde(default)]
    pub resources_base_path: String,
    #[serde(default)]
    pub version: String,
}

/// Mirrors the `predownload` section when a pre-download window is open.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PredownloadSection {
    #[serde(default)]
    pub config: LauncherConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdnNode {
    /// NOTE: Kuro uses UPPERCASE keys here (`P`, `K1`, `K2`) — not camelCase.
    #[serde(rename = "P")]
    pub p: u64,
    #[serde(rename = "K1", default)]
    pub k1: u64,
    #[serde(rename = "K2", default)]
    pub k2: u64,
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherConfig {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub index_file: String,
    #[serde(default)]
    pub index_file_md5: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub patch_type: String,
    /// One entry per *source* version; the entry whose `version` matches the
    /// locally installed version describes the incremental update.
    #[serde(default)]
    pub patch_config: Vec<PatchConfig>,
}

/// A single `oldVersion -> newVersion` incremental patch description.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchConfig {
    pub version: String,
    /// Relative path of the `PatchIndex` (JSON) on the CDN.
    pub index_file: String,
    /// Base path under which the krpdiff files for this transition live.
    pub base_url: String,
    #[serde(default)]
    pub size: u64,
}

/// The patch manifest for one source version (krpdiff groups + fallbacks).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchIndex {
    /// Full-file fallbacks; entries whose `dest` ends in `.krpdiff` are the
    /// patch payloads themselves (with size/md5 + chunkInfos for parallel fetch).
    #[serde(default)]
    pub resource: Vec<ResourceItem>,
    #[serde(default)]
    pub delete_files: Vec<String>,
    #[serde(default)]
    pub group_infos: Vec<GroupInfo>,
    #[serde(default)]
    pub apply_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceItem {
    pub dest: String,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub from_folder: Option<String>,
    #[serde(default)]
    pub chunk_infos: Vec<ChunkInfo>,
}

/// One krpdiff group: source files (old versions) + destination files (new
/// versions) that the merge produces.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    /// krpdiff filename (relative to `PatchConfig::base_url`).
    pub dest: String,
    #[serde(default)]
    pub src_files: Vec<FileRef>,
    #[serde(default)]
    pub dst_files: Vec<FileRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRef {
    pub dest: String,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub chunk_infos: Vec<ChunkInfo>,
}

/// A byte range of a file on the CDN; allows parallel ranged GETs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkInfo {
    pub start: u64,
    pub end: u64,
    #[serde(default)]
    pub md5: String,
}

/// `launcherDownloadConfig.json` — written by the official launcher and by us;
/// the single source of truth for the locally installed version/server.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalConfig {
    pub version: String,
    pub app_id: String,
    #[serde(default)]
    pub group: String,
}
