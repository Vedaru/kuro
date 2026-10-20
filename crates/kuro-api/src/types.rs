//! Serde types for the Kuro Games launcher protocol.
//!
//! Modeled on the live wire format (verified against the CN WuWa CDN, 2026-08):
//!
//! * `GET {api_url}`  -> `LauncherIndex` (per-game/per-server entry point)
//! * `GET {cdn}/{indexFile}` -> `PatchIndex`. For a *full* manifest this is the
//!   whole client; for a source version it lists what changed. Both are often
//!   plain full-file entries — WuWa CN hotfixes ship no krpdiff groups at all,
//!   and their entries carry no `fromFolder`, so a consumer must be able to fall
//!   back to the manifest's own base URL.
//!
//! All extra/mystery fields are ignored (`serde` default behavior) so Kuro can
//! add fields without breaking us.

use serde::{Deserialize, Serialize};

/// Top level of the launcher API entry point (e.g.
/// `.../launcher/game/G152/10003_<token>/index.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherIndex {
    pub default: DefaultSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultSection {
    /// Weighted CDN nodes (`P` = weight, `url` = base).
    #[serde(default)]
    pub cdn_list: Vec<CdnNode>,
    pub config: LauncherConfig,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdnNode {
    /// NOTE: Kuro uses UPPERCASE keys here (`P`) — not camelCase.
    #[serde(rename = "P")]
    pub p: u64,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherConfig {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub index_file: String,
    #[serde(default)]
    pub base_url: String,
    /// One entry per *source* version; the entry whose `version` matches the
    /// locally installed version describes the incremental update.
    #[serde(default)]
    pub patch_config: Vec<PatchConfig>,
}

/// A single `oldVersion -> newVersion` incremental patch description.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchConfig {
    pub version: String,
    /// Relative path of the `PatchIndex` (JSON) on the CDN.
    pub index_file: String,
    /// Base path under which the krpdiff files for this transition live.
    pub base_url: String,
}

/// The patch manifest for one source version (krpdiff groups + fallbacks).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    /// krpdiff filename (relative to `PatchConfig::base_url`).
    pub dest: String,
    #[serde(default)]
    pub src_files: Vec<FileRef>,
    #[serde(default)]
    pub dst_files: Vec<FileRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The 3.0+ launcher's quality-pack document (`…/<appId>_<token>/G<id>/official/index.json`).
///
/// This is a *different* endpoint from [`LauncherIndex`]: not the game version
/// (which the channel's `index.json` + `patchConfig` describes) but the set of
/// optional resource packs, one per [`Quality`] tier. Each pack carries its own
/// version, manifest and CDN — and, crucially, its own file list, which the
/// channel's full manifest does not have to include.
///
/// Verified 2026-10 against WuWa CN 3.7.1: `resourcePacks` is `{uhd, sd, hd,
/// common}` at ~66.0 / 22.0 / 45.7 / 40.7 GB, and the three tier manifests are
/// 100 files each under `Client/Content/<TIER>/`, disjoint from `common`'s 816.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackConfig {
    #[serde(default)]
    pub cdn_list: Vec<PackCdnNode>,
    /// Keyed by lowercase pack name (`common`, `sd`, `hd`, `uhd`).
    #[serde(default)]
    pub resource_packs: std::collections::BTreeMap<String, PackEntry>,
    /// Keyed by UPPERCASE tier (`SD`, `HD`, `UHD`): which packs that tier is
    /// made of. `HD` -> `[common, hd]`, and so on.
    #[serde(default)]
    pub bundles: std::collections::BTreeMap<String, PackBundle>,
}

/// One CDN edge from a [`PackConfig`].
///
/// `K1`/`K2` are the launcher's enable flags; a node with either at 0 is not
/// serving pack traffic. `P` is the weight, as in [`CdnNode`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackCdnNode {
    #[serde(rename = "P", default)]
    pub p: u64,
    pub url: String,
    /// Absent means "not disabled": only an explicit 0 excludes the edge.
    #[serde(rename = "K1", default)]
    pub k1: Option<u8>,
    #[serde(rename = "K2", default)]
    pub k2: Option<u8>,
}

impl PackCdnNode {
    pub fn enabled(&self) -> bool {
        self.p > 0 && self.k1.unwrap_or(1) != 0 && self.k2.unwrap_or(1) != 0
    }
}

/// One quality pack: its version, manifest and download root.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackEntry {
    #[serde(default)]
    pub version: String,
    /// CDN-relative path of the pack's manifest, a [`PatchIndex`].
    #[serde(default)]
    pub index_file: String,
    /// CDN-relative root the pack's files are served from.
    #[serde(default)]
    pub base_url: String,
    /// Total packed size, as the launcher reports it.
    #[serde(default)]
    pub size: u64,
    /// MD5 of the manifest itself; informational (the manifest's own entries
    /// carry the per-file hashes that are actually enforced).
    #[serde(default)]
    pub index_file_md5: String,
}

/// A tier's composition, e.g. `HD -> {resourcePacks: [common, hd]}`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackBundle {
    #[serde(default)]
    pub resource_packs: Vec<String>,
}

/// `launcherDownloadConfig.json` — written by the official launcher and by us;
/// the single source of truth for the locally installed version/server.
///
/// This file belongs to the official launcher, and it holds more than the three
/// fields below: at minimum `bundles` (which quality packs that launcher
/// considers installed) and whatever the launcher adds next. Rewriting it from
/// this struct alone would drop those, and dropping `bundles` is not cosmetic —
/// the launcher then reports the pack as missing / offers "repair". So every
/// unknown key round-trips in `extra`, and callers must serialise the whole
/// struct rather than build a fresh one from a version string.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalConfig {
    pub version: String,
    pub app_id: String,
    #[serde(default)]
    pub group: String,
    /// Every key we do not model, preserved verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl LocalConfig {
    /// A fresh config for a first install, with no keys to preserve.
    pub fn new(version: impl Into<String>, app_id: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            app_id: app_id.into(),
            group: "default".to_string(),
            extra: serde_json::Map::new(),
        }
    }

    /// The same config with the version/appId of a different run, keeping every
    /// key we do not model (see the type-level note on `bundles`).
    pub fn retargeted(&self, version: impl Into<String>, app_id: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            app_id: app_id.into(),
            group: "default".to_string(),
            extra: self.extra.clone(),
        }
    }
}
