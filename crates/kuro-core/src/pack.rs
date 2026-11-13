//! Quality *packs* — installing `Client/Content/<TIER>/` and registering it
//! with the official launcher.
//!
//! [`crate::quality`] covers what a tier *is* (the `-krqlv=` argument, the
//! directory the client mounts) and which body a *manifest* offers. This module
//! covers where the files come from and what the launcher must be told
//! afterwards.
//!
//! # Why the channel manifest is not enough
//!
//! WuWa 3.7's channel manifests (CN `G152`, Bilibili `G152`, Global `G153`)
//! carry the base paks plus exactly one body, `Client/Content/HD/`, 100 files.
//! SD and UHD are absent entirely, so `--quality sd` has nothing to fetch and
//! `narrow_to_body` cannot help. The 3.0+ launcher has a second document for
//! this — the *pack config* ([`kuro_api::PackConfig`], served from
//! `…/<appId>_<token>/G<id>/official/index.json`) — which lists every pack with
//! its own version, manifest and CDN. Verified 2026-10 on 3.7.1:
//!
//! | pack | size | files | where |
//! |---|---|---|---|
//! | `common` | 37.95 GiB | 816 | binaries, base paks, engine, wwise |
//! | `sd` | 20.46 GiB | 100 | `Client/Content/sd` |
//! | `hd` | 42.57 GiB | 100 | `Client/Content/hd` |
//! | `uhd` | 61.68 GiB | 100 | `Client/Content/uhd` |
//!
//! and `bundles` maps a tier to the packs it needs (`HD` → `[common, hd]`).
//! Tier bodies are disjoint from each other and `common ∩ full` is identical
//! (`hd` happens to duplicate the channel's HD body byte for byte).
//!
//! Since 3.8.0 a pack manifest also carries optional map/event packs under
//! `Client/Saved/Resources/<pack version>/`. The official rule is exactly one
//! rule: a tier writes its body or any path under the target version's resource
//! directory, and `common` writes anything outside a tier body. The version
//! directory is the only discriminator. The rest of `Client/Saved` (Config,
//! LocalStorage, the client's own Resource/Lang/Video directories) is live game
//! state and stays off limits.
//!
//! `common` is the base client, not a second source for a tier. A tier install
//! (`kuro pack`) never touches it. The game/pack update applies `common` and
//! the active tier together through their own `patchConfig` deltas, then writes
//! the new version once both have landed.
//!
//! # Why the launcher must be told
//!
//! Files alone do not make a tier "installed". The launcher reads two records:
//!
//! 1. `bundles.<TIER>` in the root `launcherDownloadConfig.json` — without it,
//!    a directory redirect reports *"redirect bundle incompatible"*;
//! 2. `launcherDownloadConfig/<tier>.json` (`packName`, `version`) — without
//!    it, the play button turns into *"repair game"* (`KRCheckUpdateFlow: pack
//!    version file missing`).
//!
//! Both are written only after every file of the tier verifies, by
//! [`register`], which round-trips every key it does not model (see
//! [`LocalConfig::extra`]). The reverse is just as important: a (re)install
//! withdraws them with [`unregister`] before the first byte is written, so a
//! run that dies halfway leaves "not installed" instead of a record claiming
//! files that are now partly gone.
//!
//! # What is deliberately different from `ww-manager`
//!
//! * No plan file and no staged apply. Files land in place through the same
//!   temp-sibling + [`safe_replace`] path `sync` uses, and a rerun resumes by
//!   MD5 — an interrupted tier is harmless, because the client only mounts a
//!   body directory when launched with that tier.
//! * A tier install is just `resourcePacks[<tier>]`. `common` is the base
//!   client, so a tier install never rewrites it. This is the official
//!   `resource` operation, and it is why a tier install cannot clobber a
//!   channel-specific base file.
//! * Verification runs *before* anything is written, as one scan phase over
//!   every pack, so `--check` and a repair agree on what is missing and a
//!   repair with nothing to do does not rewrite records the launcher may have
//!   touched.
//! * The tier's files are pruned against the previous run's own list
//!   ([`PackRecord::files`]) when its manifest shrinks — only paths kuro wrote
//!   are candidates, so a hand-copied pak survives.
//! * The manifest itself is checked against the `indexFileMd5` the pack
//!   document publishes, and an offered version older than the installed one is
//!   refused rather than silently rolled back.
//!
//! # The update path
//!
//! [`GameManager::pack_update`] is the official launcher's incremental path:
//! read `resourcePacks.<name>.patchConfig` for `common` and the active tier,
//! download the krpdiff groups and full-file fallbacks, apply `common` first
//! then the tier, and only then write `launcherDownloadConfig.version =
//! resourcePacks.common.version`. A missing source-version delta is a refusal,
//! never a full-body fallback or a switch to another tier.
//!
//! The two phases are also exposed separately. [`GameManager::pack_download`]
//! stages into `.incremental_download/packs/<name>` (one directory per pack,
//! so their `.krpdiff` names can never collide) and writes a resumable
//! `plan.json`. [`GameManager::pack_apply`] applies that plan and only then
//! writes the version. The pack document's nested `predownload` block is the
//! next version's pack set: it is used only once the formal `common` and tier
//! versions are installed, matching `ww-manager`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kuro_api::{ApiClient, Error, PackConfig, PackEntry, PatchIndex, Result, ServerEntry};

use crate::atomic::safe_replace;
use crate::download::{download_file, FILE_CONCURRENCY};
use crate::game::{is_krpdiff, part_already_target};
use crate::md5_cache::Md5Cache;
use crate::path::{key as path_key, normalise, tmp_sibling};
use crate::quality::{self, Quality};
use crate::state;

/// `launcherDownloadConfig/` — the launcher's per-pack version records.
pub const PACKS_DIR: &str = "launcherDownloadConfig";

/// What one pack contributed.
#[derive(Debug, Clone)]
pub struct PackPartReport {
    pub name: String,
    pub version: String,
    pub files: usize,
    /// Files written this run (a resumed run only writes what was missing).
    pub downloaded: usize,
    pub bytes: u64,
    /// Files the channel manifest owns differently, left untouched.
    pub skipped: usize,
}

/// Summary of installing (or verifying) one tier.
#[derive(Debug, Clone)]
pub struct PackReport {
    pub tier: Quality,
    pub parts: Vec<PackPartReport>,
    /// Whether the launcher holds valid records for this tier after this run —
    /// either written now, or already correct and left alone.
    pub registered: bool,
    /// Files reported as broken but not repaired (`--check`).
    pub broken: Vec<String>,
    /// Files of a previous run that this manifest no longer lists, deleted.
    pub pruned: usize,
}

impl PackReport {
    pub fn bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes).sum()
    }
}

/// One pack's share of an official `patchConfig` update.
#[derive(Debug, Clone)]
pub struct PackUpdatePart {
    pub name: String,
    pub from_version: String,
    pub to_version: String,
    pub report: crate::ApplyReport,
}

/// Summary of a `common` + tier pack update.
#[derive(Debug, Clone, Default)]
pub struct PackUpdateReport {
    /// The game version recorded after the update, i.e. `common`'s target.
    pub version: String,
    /// True when this came from the pack document's nested `predownload`
    /// block, not the formal packs.
    pub predownload: bool,
    pub parts: Vec<PackUpdatePart>,
}

/// What a pack update would download, without writing anything.
#[derive(Debug, Clone, Default)]
pub struct PackUpdatePlan {
    pub version: String,
    pub predownload: bool,
    pub parts: Vec<PackUpdatePlanPart>,
}

#[derive(Debug, Clone)]
pub struct PackUpdatePlanPart {
    pub name: String,
    pub from_version: String,
    pub to_version: String,
    pub files: usize,
    pub bytes: u64,
}

/// What an installed tier removal freed.
#[derive(Debug, Clone)]
pub struct PackRemoveReport {
    pub tier: Quality,
    pub files: usize,
    pub bytes: u64,
}

/// One pack's delta the official update path would apply.
#[derive(Debug, Clone)]
struct PackUpdateStep {
    name: String,
    from: String,
    to: String,
    patch: kuro_api::PackPatch,
}

const PACK_PLAN_FILE: &str = "plan.json";

/// A downloaded, not-yet-applied pack update. The indexes are persisted so the
/// apply step verifies the exact bytes the download step resolved, even if the
/// CDN republishes in between.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PackPlanFile {
    schema: u32,
    server: String,
    tier: String,
    bases: Vec<String>,
    bundle: Vec<String>,
    predownload: bool,
    ready: bool,
    parts: Vec<PackPlanPart>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PackPlanPart {
    name: String,
    from: String,
    to: String,
    patch: kuro_api::PackPatch,
    index: PatchIndex,
}

fn pack_staging_dir(game_folder: &Path, name: &str) -> PathBuf {
    state::incremental_dir(game_folder).join("packs").join(name)
}

fn pack_plan_path(game_folder: &Path) -> PathBuf {
    state::incremental_dir(game_folder).join("packs").join(PACK_PLAN_FILE)
}

/// True when a completed predownload is waiting to be applied.
pub fn plan_ready(game_folder: &Path) -> bool {
    std::fs::read_to_string(pack_plan_path(game_folder))
        .ok()
        .and_then(|data| serde_json::from_str::<PackPlanFile>(&data).ok())
        .is_some_and(|plan| plan.ready && plan.parts.iter().all(|part| part_ready(game_folder, part)))
}

fn part_ready(game_folder: &Path, part: &PackPlanPart) -> bool {
    let stage = pack_staging_dir(game_folder, &part.name);
    part.index
        .resource
        .iter()
        .all(|r| std::fs::metadata(stage.join(r.dest.trim_start_matches('/'))).is_ok_and(|m| m.len() == r.size))
}

fn tree_totals(root: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let Ok(rd) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    for entry in rd.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let (f, b) = tree_totals(&entry.path());
            files += f;
            bytes += b;
        } else if kind.is_file() {
            files += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (files, bytes)
}

/// The pack document's predownload block is the *next* version's pack set.
/// The formal packs stay the priority while they still have a delta, exactly
/// like `ww-manager`; only once `common` and the active tier are at the formal
/// versions does the nested block become the target.
fn effective_pack_config<'a>(
    config: &'a PackConfig,
    tier: Quality,
    local: &str,
    current_tier: &str,
) -> Result<(&'a PackConfig, bool)> {
    let formal_current = config.resource_packs.get("common").is_some_and(|e| e.version == local)
        && config
            .resource_packs
            .get(&tier.pack_key())
            .is_some_and(|e| e.version == current_tier);
    if formal_current {
        if let Some(pre) = &config.predownload {
            if pre.resource_packs.is_empty() {
                return Err(Error::Patch(
                    "the pack document's predownload block has no resourcePacks".into(),
                ));
            }
            if !pre.bundles.contains_key(tier.as_arg()) {
                return Err(Error::Patch(format!(
                    "the predownload block has no bundle for {}",
                    tier.as_arg()
                )));
            }
            return Ok((pre.as_ref(), true));
        }
    }
    Ok((config, false))
}

/// Build the `common` + tier delta list. A missing `patchConfig` entry is a
/// refusal, never a full-body fallback.
fn pack_update_steps(
    tier: Quality,
    config: &PackConfig,
    local: &str,
    current_tier: &str,
) -> Result<Vec<PackUpdateStep>> {
    let tier_key = tier.pack_key();
    let common_entry = config
        .resource_packs
        .get("common")
        .ok_or_else(|| Error::Patch("the pack document has no `common` pack".to_string()))?;
    let tier_entry = config.resource_packs.get(&tier_key).ok_or_else(|| {
        Error::Patch(format!(
            "the pack document for this channel has no `{tier_key}` pack (has: {})",
            config.resource_packs.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;
    let mut work = Vec::new();
    for (name, from, entry) in [
        ("common".to_string(), local.to_string(), common_entry.clone()),
        (tier_key, current_tier.to_string(), tier_entry.clone()),
    ] {
        let to = entry.version.clone();
        if from == to {
            continue;
        }
        if is_downgrade(&from, &to) {
            return Err(Error::Patch(format!(
                "the channel offers {name} {to}, older than the installed {from} — \
                 refusing to downgrade; the pack document is probably stale"
            )));
        }
        let patch = entry
            .patch_config
            .iter()
            .find(|p| p.version == from)
            .cloned()
            .ok_or_else(|| {
                Error::Patch(format!(
                    "`{name}` has no {from} → {to} patchConfig entry — the official launcher \
                     does not install a full body here; wait for the channel to publish it"
                ))
            })?;
        if patch.index_file.is_empty() {
            return Err(Error::MissingField("patchConfig.indexFile"));
        }
        if patch.base_url.is_empty() {
            return Err(Error::MissingField("patchConfig.baseUrl"));
        }
        work.push(PackUpdateStep { name, from, to, patch });
    }
    // `common` first, then the tier. The version record is written only after
    // both, exactly like the official launcher.
    work.sort_by_key(|p| usize::from(p.name != "common"));
    Ok(work)
}

/// The installed version of a tier, or an error if it was never installed.
///
/// Old installs predate the launcher's tier records and use HD by default, so
/// HD falls back to the game version. SD/UHD must have a record.
fn installed_tier_version(game_folder: &Path, tier: Quality, local: &str) -> Result<String> {
    let tier_key = tier.pack_key();
    let state = state_of(game_folder, tier);
    state
        .marker
        .as_ref()
        .map(|m| m.version.clone())
        .or_else(|| read_record(game_folder).installed.get(&tier_key).cloned())
        .or_else(|| (tier == Quality::Hd).then(|| local.to_string()))
        .ok_or_else(|| {
            Error::Patch(format!(
                "no installed {} pack — run `kuro pack <folder> {}` first",
                tier.as_arg(),
                tier.as_arg().to_ascii_lowercase()
            ))
        })
}

/// A tier as recorded on disk, for `kuro pack` and the TUI.
#[derive(Debug, Clone, Default)]
pub struct PackState {
    /// `launcherDownloadConfig/<tier>.json` exists.
    pub marker: Option<PackMarker>,
    /// `bundles.<TIER>` exists in the root config.
    pub in_bundles: bool,
    /// Version this tool last installed (our own record, not the launcher's).
    pub recorded_version: Option<String>,
}

impl PackState {
    /// The launcher will accept this tier: both records are present.
    pub fn registered(&self) -> bool {
        self.marker.is_some() && self.in_bundles
    }
}

/// Contents of `launcherDownloadConfig/<tier>.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackMarker {
    #[serde(rename = "packName")]
    pub pack_name: String,
    pub version: String,
}

/// Our own record of installed packs, in `.kuro_cache/packs.json`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PackRecord {
    /// Pack name -> version, e.g. `common` -> `3.7.1`.
    #[serde(default)]
    pub installed: std::collections::BTreeMap<String, String>,
    /// Pack name -> the manifest `dest`s that pack wrote. A reinstall prunes
    /// only paths in this list, so files the user put in a body directory by
    /// hand are never deleted.
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, Vec<String>>,
}

pub fn record_path(game_folder: &Path) -> PathBuf {
    state::cache_dir(game_folder).join("packs.json")
}

pub fn read_record(game_folder: &Path) -> PackRecord {
    std::fs::read_to_string(record_path(game_folder))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_record(game_folder: &Path, record: &PackRecord) -> Result<()> {
    state::write_atomic(&record_path(game_folder), &serde_json::to_string_pretty(record)?)
}

/// Read the launcher's records for one tier.
pub fn state_of(game_folder: &Path, tier: Quality) -> PackState {
    let marker = std::fs::read_to_string(game_folder.join(PACKS_DIR).join(format!("{}.json", tier.pack_key())))
        .ok()
        .and_then(|s| serde_json::from_str::<PackMarker>(&s).ok());
    let in_bundles = state::read_local_config(game_folder)
        .ok()
        .flatten()
        .and_then(|c| c.extra.get("bundles").cloned())
        .and_then(|b| b.get(tier.as_arg()).cloned())
        .is_some();
    PackState {
        marker,
        in_bundles,
        recorded_version: read_record(game_folder).installed.get(&tier.pack_key()).cloned(),
    }
}

/// Write both launcher records for `tier`, preserving everything we do not
/// model in the root config.
///
/// Runs only after every file of the tier has verified: a record that claims a
/// pack the disk does not have is exactly the state the launcher answers with
/// "repair game", so the order matters.
pub fn register(
    game_folder: &Path,
    tier: Quality,
    version: &str,
    bundle_packs: Option<&[String]>,
    files: Option<&[String]>,
) -> Result<()> {
    // A tier cannot be installed into a folder with no config: that file is the
    // install's identity, and kuro has no appId to invent for it.
    let mut cfg =
        state::read_local_config(game_folder)?.ok_or_else(|| Error::NoLocalConfig(game_folder.to_path_buf()))?;

    let packs: Vec<String> = bundle_packs
        .map(|p| p.to_vec())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| vec!["common".to_string(), tier.pack_key()]);
    let mut bundles = match cfg.extra.remove("bundles") {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    bundles.insert(tier.as_arg().to_string(), bundle_record(version, &packs));
    cfg.extra
        .insert("bundles".to_string(), serde_json::Value::Object(bundles));
    state::write_local_config(game_folder, &cfg)?;

    let dir = game_folder.join(PACKS_DIR);
    std::fs::create_dir_all(&dir)?;
    let marker = PackMarker {
        pack_name: tier.pack_key(),
        version: version.to_string(),
    };
    let marker_path = dir.join(format!("{}.json", tier.pack_key()));
    state::write_atomic(&marker_path, &serde_json::to_string_pretty(&marker)?)?;

    let mut record = read_record(game_folder);
    let key = tier.pack_key();
    record.installed.insert(key.clone(), version.to_string());
    if let Some(files) = files {
        record.files.insert(key, files.to_vec());
    }
    write_record(game_folder, &record)?;
    Ok(())
}

/// Withdraw a tier's launcher records.
///
/// Called before the first byte of a (re)install is written, so a run that dies
/// halfway leaves "not installed" rather than a record claiming files that are
/// no longer all there. The root config keeps `common`'s version: that is the
/// base client, which this does not touch.
pub fn unregister(game_folder: &Path, tier: Quality) -> Result<()> {
    let marker = game_folder.join(PACKS_DIR).join(format!("{}.json", tier.pack_key()));
    if marker.exists() {
        std::fs::remove_file(&marker)?;
    }
    let Some(mut cfg) = state::read_local_config(game_folder)? else {
        return Ok(());
    };
    let Some(serde_json::Value::Object(mut bundles)) = cfg.extra.remove("bundles") else {
        return Ok(());
    };
    if bundles.remove(tier.as_arg()).is_none() {
        cfg.extra
            .insert("bundles".to_string(), serde_json::Value::Object(bundles));
        return Ok(());
    }
    cfg.extra
        .insert("bundles".to_string(), serde_json::Value::Object(bundles));
    state::write_local_config(game_folder, &cfg)?;
    Ok(())
}

/// Delete files this tier's previous run wrote that its manifest no longer
/// lists, and prune the directories that leaves empty.
///
/// Only paths in [`PackRecord::files`] are candidates: a file the user dropped
/// into `Client/Content/HD/` themselves is not ours to remove. Empty
/// directories are pruned because a tier whose pak set shrank would otherwise
/// leave a tree of empty folders behind.
fn prune_stale(game_folder: &Path, tier: Quality, keep: &std::collections::HashSet<String>) -> usize {
    let key = tier.pack_key();
    let mut record = read_record(game_folder);
    let Some(previous) = record.files.get(&key).cloned() else {
        return 0;
    };
    let mut removed = 0;
    let mut parents = std::collections::BTreeSet::new();
    for dest in &previous {
        if keep.contains(&path_key(dest)) {
            continue;
        }
        let path = game_folder.join(normalise(dest));
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            removed += 1;
            let mut dir = path.parent();
            while let Some(d) = dir {
                if d == game_folder || !d.starts_with(game_folder) {
                    break;
                }
                parents.insert(d.to_path_buf());
                dir = d.parent();
            }
        }
    }
    // Deepest first, so a directory emptied by removing a file in its only
    // subdirectory can itself go.
    for dir in parents.iter().rev() {
        let _ = std::fs::remove_dir(dir);
    }
    if removed > 0 {
        record.files.remove(&key);
        let _ = write_record(game_folder, &record);
    }
    removed
}

/// Is `offered` an older version than `installed`?
///
/// Only dotted-numeric versions are comparable (the WuWa packs use `3.7.1`).
/// Anything else — a build id, a channel-suffixed string — answers `false`:
/// refusing to install on an unparseable pair would strand users on a version
/// the channel has legitimately moved past.
fn is_downgrade(installed: &str, offered: &str) -> bool {
    fn parse(v: &str) -> Option<Vec<u64>> {
        if v.is_empty() {
            return None;
        }
        v.split('.').map(|p| p.parse::<u64>().ok()).collect()
    }
    let (Some(current), Some(offered)) = (parse(installed), parse(offered)) else {
        return false;
    };
    // `3.7` and `3.7.0` are the same version, so a shorter prefix is padded.
    let len = current.len().max(offered.len());
    let pad = |mut v: Vec<u64>| {
        v.resize(len, 0);
        v
    };
    pad(offered) < pad(current)
}

/// Every version that still says something about what is installed.
///
/// The launcher's marker (`launcherDownloadConfig/<tier>.json`) is written by
/// the official updater, which knows nothing about kuro's record, so it is the
/// one that catches "user updated through the launcher". Our own
/// (`.kuro_cache/packs.json`) is only evidence while the root config still
/// lists the bundle: `unregister` deliberately leaves the version behind, and
/// an uninstalled tier must not be able to block a legitimate older reinstall.
fn installed_versions(game_folder: &Path, tier: Quality) -> Vec<String> {
    let state = state_of(game_folder, tier);
    let mut seen: Vec<String> = state.marker.iter().map(|m| m.version.clone()).collect();
    if state.in_bundles {
        if let Some(v) = read_record(game_folder).installed.get(&tier.pack_key()) {
            if !seen.contains(v) {
                seen.push(v.clone());
            }
        }
    }
    seen
}

/// One pack of a tier, resolved and validated, ready to scan.
struct PlannedPack {
    name: String,
    entry: PackEntry,
    manifest: PatchIndex,
}

/// The manifest of the tier's own pack, not `common`.
///
/// `common` overlaps the channel manifest, which `sync` owns, so its paths must
/// never become this module's pruning candidates or its file record.
fn tier_manifest(plan: &[PlannedPack], tier: Quality) -> Option<&PatchIndex> {
    plan.iter()
        .find(|p| p.name.eq_ignore_ascii_case(tier.as_arg()))
        .map(|p| &p.manifest)
}

/// Drop files of the previous install that the tier's manifest no longer lists,
/// and return the dests it does list — the record the next run prunes against.
///
/// `plan_pack_tier` has already refused an empty tier manifest, so the list here
/// is always the published one; an empty `keep` would only mean "no plan for
/// this tier", where nothing is pruned and no record is returned.
fn prune_and_record(game_folder: &Path, tier: Quality, plan: &[PlannedPack]) -> (usize, Vec<String>) {
    let Some(manifest) = tier_manifest(plan, tier).filter(|m| manifest_is_authoritative(m)) else {
        return (0, Vec::new());
    };
    let keep: std::collections::HashSet<String> = manifest.resource.iter().map(|r| path_key(&r.dest)).collect();
    let pruned = prune_stale(game_folder, tier, &keep);
    let dests = manifest.resource.iter().map(|r| r.dest.clone()).collect();
    (pruned, dests)
}

/// Whether a manifest may be used as the *complete* list of a tier's files.
///
/// It may not when it is empty. An empty index is not "this tier ships nothing",
/// it is a document that failed to be published or was replaced by a stub (all
/// three live 3.7.0 tier indexes are 41-byte `{"resource":[]}` stubs). Pruning
/// against one deletes the whole installed tier — tens of GiB — and then
/// registers the now-empty tier as installed, so the launcher sees no problem
/// to repair. Requiring a non-empty manifest costs nothing in the normal case:
/// every real tier ships 100 files.
fn manifest_is_authoritative(manifest: &PatchIndex) -> bool {
    !manifest.resource.is_empty()
}

/// A digest in the form the CDN publishes: 32 hex digits.
fn is_md5(md5: &str) -> bool {
    md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Settle the launcher's records for a tier that is now correct on disk, and
/// return how many stale files were pruned.
///
/// A healthy registration is left alone: the launcher may have rewritten it
/// while the game ran, and replacing a working record with ours buys nothing.
fn reconcile_records(
    game_folder: &Path,
    tier: Quality,
    plan: &[PlannedPack],
    version: &str,
    bundle: &[String],
) -> Result<usize> {
    let (pruned, files) = prune_and_record(game_folder, tier, plan);
    let state = state_of(game_folder, tier);
    let current = state.marker.as_ref().is_some_and(|m| m.version == version);
    if !state.registered() || !current {
        // An empty list would overwrite the only description of what is on
        // disk with "nothing", stranding those files beyond any future prune.
        let files = (!files.is_empty()).then_some(files.as_slice());
        register(game_folder, tier, version, Some(bundle), files)?;
    }
    Ok(pruned)
}

/// The version a tier records: its own pack's, or `common`'s when the document
/// names none for the tier.
fn planned_version(plan: &[PlannedPack], tier: Quality, config: &PackConfig) -> String {
    plan.iter()
        .find(|p| p.name.eq_ignore_ascii_case(tier.as_arg()))
        .map(|p| p.entry.version.clone())
        .or_else(|| config.resource_packs.get("common").map(|c| c.version.clone()))
        .unwrap_or_default()
}

fn bundle_record(version: &str, packs: &[String]) -> serde_json::Value {
    serde_json::json!({
        "version": version,
        "state": "",
        "resourcePacks": packs,
    })
}

/// Which packs a tier is made of, from the channel's own `bundles` map.
fn bundle_packs(config: &PackConfig, tier: Quality) -> Vec<String> {
    config
        .bundles
        .get(tier.as_arg())
        .map(|b| b.resource_packs.clone())
        .unwrap_or_default()
}

impl crate::GameManager {
    /// The channel's pack document, or an error naming the channel when it has
    /// none (Global today).
    pub async fn pack_config(&self) -> Result<PackConfig> {
        let entry: &ServerEntry = self.server_entry();
        self.api_client().fetch_pack_config(entry.pack_config_urls).await
    }

    /// Verify a tier and download whatever is missing, then register it with
    /// the launcher. `repair = false` reports without touching anything.
    pub async fn sync_pack(
        &self,
        tier: Quality,
        repair: bool,
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackReport> {
        if !self.game.uses_quality_tiers() {
            return Err(Error::Unsupported(
                "this game has no quality packs — only Wuthering Waves has Client/Content/<TIER>",
            ));
        }
        let config = self.pack_config().await?;
        let bases: Vec<String> = ApiClient::pack_cdn_candidates(&config)?
            .iter()
            .map(|n| n.url.clone())
            .collect();
        self.sync_pack_inner(tier, &config, &bases, repair, tx).await
    }

    /// [`GameManager::sync_pack`] with the network discovery already done, so
    /// the install path is testable against a local server.
    pub async fn sync_pack_inner(
        &self,
        tier: Quality,
        config: &PackConfig,
        bases: &[String],
        repair: bool,
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackReport> {
        if bases.is_empty() {
            return Err(Error::NoCdnNode);
        }
        let plan = self.plan_pack_tier(tier, config, bases, tx.as_ref()).await?;
        let version = planned_version(&plan, tier, config);
        let bundle = bundle_packs(config, tier);
        for installed in installed_versions(&self.game_folder, tier) {
            if is_downgrade(&installed, &version) {
                return Err(Error::Patch(format!(
                    "the channel offers {} {version}, older than the installed {installed} — \
                     refusing to downgrade; the pack document is probably stale",
                    tier.as_arg()
                )));
            }
        }

        // Verify first, write second. Every pack is scanned before anything is
        // touched, so `--check` reports the whole tier and a repair that has
        // nothing to do never withdraws the records it is about to rewrite.
        // One cache for the whole tier: `save` drops entries that were not
        // touched, so a per-part cache/save would evict the other part's hashes
        // and force a full re-hash of tens of GiB on every run.
        let cache = Md5Cache::load(&self.game_folder);
        let mut scans = Vec::with_capacity(plan.len());
        for planned in &plan {
            scans.push(
                self.scan_pack_part(&planned.name, &planned.entry, &planned.manifest, &cache, tx.as_ref())
                    .await?,
            );
        }

        let mut report = PackReport {
            tier,
            parts: scans.iter().map(|s| s.report.clone()).collect(),
            registered: false,
            broken: Vec::new(),
            pruned: 0,
        };

        if !repair {
            // `--check`: the scan already holds exactly the entries that failed
            // size + MD5, so reporting them costs nothing further.
            report.broken = scans
                .iter()
                .flat_map(|s| s.todo.iter().map(|m| m.item.dest.clone()))
                .collect();
            for (part, scan) in report.parts.iter_mut().zip(&scans) {
                part.downloaded = scan.todo.len();
                part.bytes = scan.todo.iter().map(|m| m.item.size).sum();
            }
            return Ok(report);
        }

        if scans.iter().all(|s| s.todo.is_empty()) {
            let _ = cache.save();
            report.pruned = reconcile_records(&self.game_folder, tier, &plan, &version, &bundle)?;
            report.registered = true;
            return Ok(report);
        }

        // The tier is about to change on disk, so withdraw its launcher records
        // *before* the first byte: a run that dies halfway must leave "not
        // installed", not a record claiming packs that are now partly missing.
        // That is exactly the state the launcher answers with "repair game".
        unregister(&self.game_folder, tier)?;

        for (idx, scan) in scans.into_iter().enumerate() {
            // Saved after each part so an interrupted tier keeps the hashes of
            // the parts that did finish. Safe only because `save` snapshots
            // `touched` instead of draining it — with a draining save, this
            // second call would evict the first part's hashes.
            let part = self
                .write_pack_part(scan, &plan[idx].entry, bases, &cache, tx.as_ref())
                .await?;
            let _ = cache.save();
            report.broken.extend(part.broken);
            report.parts[idx] = part.report;
        }

        if report.broken.is_empty() {
            report.pruned = reconcile_records(&self.game_folder, tier, &plan, &version, &bundle)?;
            report.registered = true;
        }
        Ok(report)
    }

    /// The official launcher's pack update: `common` **and** the active tier,
    /// each through its own `patchConfig` delta, downloaded together and
    /// applied together.
    ///
    /// This is the one-step form used by `kuro update`. The TUI exposes the
    /// two phases separately: [`Self::pack_download`] then [`Self::pack_apply`].
    pub async fn pack_update(
        &self,
        tier: Quality,
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackUpdateReport> {
        let plan = self.pack_download(tier, tx.clone()).await?;
        if plan.parts.is_empty() {
            return Ok(PackUpdateReport {
                version: plan.version,
                predownload: plan.predownload,
                parts: Vec::new(),
            });
        }
        self.pack_apply(tx).await
    }

    /// [`GameManager::pack_update`] with the pack document and CDN already
    /// resolved, so the orchestration is testable against a local server.
    pub async fn pack_update_inner(
        &self,
        tier: Quality,
        config: &PackConfig,
        bases: &[String],
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackUpdateReport> {
        let plan = self.pack_download_inner(tier, config, bases, tx.clone()).await?;
        if plan.parts.is_empty() {
            return Ok(PackUpdateReport {
                version: plan.version,
                predownload: plan.predownload,
                parts: Vec::new(),
            });
        }
        self.pack_apply(tx).await
    }

    /// Stage the `common` + tier deltas without touching the game folder.
    ///
    /// The plan and the staged payloads live under
    /// `.incremental_download/packs/`, one directory per pack, so `common` and
    /// `hd` can never overwrite each other's `.krpdiff` files.
    pub async fn pack_download(
        &self,
        tier: Quality,
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackUpdatePlan> {
        if !self.game.uses_quality_tiers() {
            return Err(Error::Unsupported(
                "this game has no quality packs — only Wuthering Waves has Client/Content/<TIER>",
            ));
        }
        let config = self.pack_config().await?;
        let bases: Vec<String> = ApiClient::pack_cdn_candidates(&config)?
            .iter()
            .map(|n| n.url.clone())
            .collect();
        self.pack_download_inner(tier, &config, &bases, tx).await
    }

    /// [`GameManager::pack_download`] with an injected document and CDN.
    pub async fn pack_download_inner(
        &self,
        tier: Quality,
        config: &PackConfig,
        bases: &[String],
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackUpdatePlan> {
        if bases.is_empty() {
            return Err(Error::NoCdnNode);
        }
        let local = self
            .local_version()?
            .ok_or_else(|| Error::NoLocalConfig(self.game_folder.clone()))?;
        let current_tier = installed_tier_version(&self.game_folder, tier, &local)?;
        let (effective, is_predownload) = effective_pack_config(config, tier, &local, &current_tier)?;
        let work = pack_update_steps(tier, effective, &local, &current_tier)?;

        if work.is_empty() {
            let _ = std::fs::remove_dir_all(state::incremental_dir(&self.game_folder).join("packs"));
            return Ok(PackUpdatePlan {
                version: local,
                predownload: is_predownload,
                parts: Vec::new(),
            });
        }

        let common_target = work
            .iter()
            .find(|p| p.name == "common")
            .map(|p| p.to.clone())
            .unwrap_or_else(|| local.clone());
        let mut plan_file = PackPlanFile {
            schema: 1,
            server: self.server.to_string(),
            tier: tier.as_arg().to_string(),
            bases: bases.to_vec(),
            bundle: bundle_packs(effective, tier),
            predownload: is_predownload,
            ready: false,
            parts: Vec::new(),
        };
        let base_refs: Vec<&str> = bases.iter().map(String::as_str).collect();
        for part in &work {
            let index = self
                .api_client()
                .fetch_manifest_verified(&base_refs, &part.patch.index_file, &part.patch.index_file_md5)
                .await?;
            validate_pack_update_index(&part.name, &part.to, &index)?;
            plan_file.parts.push(PackPlanPart {
                name: part.name.clone(),
                from: part.from.clone(),
                to: part.to.clone(),
                patch: part.patch.clone(),
                index,
            });
        }
        let plan_path = pack_plan_path(&self.game_folder);
        state::write_atomic(&plan_path, &serde_json::to_string_pretty(&plan_file)?)?;

        let mut report = PackUpdatePlan {
            version: common_target,
            predownload: is_predownload,
            parts: Vec::new(),
        };
        // Build every part's plan before downloading so the UI gets one total
        // for `common` + the active tier, instead of a bar that resets when
        // the second pack starts.
        let mut planned = Vec::with_capacity(plan_file.parts.len());
        for part in &plan_file.parts {
            let stage = pack_staging_dir(&self.game_folder, &part.name);
            let patch_cfg = kuro_api::PatchConfig {
                version: part.from.clone(),
                index_file: part.patch.index_file.clone(),
                base_url: part.patch.base_url.clone(),
            };
            let part_plan = self.plan_from_patch_index_in(&part.index, &part.from, &part.to, &stage);
            if let Some(tx) = &tx {
                let _ = tx
                    .send(crate::ProgressEvent::Log(format!(
                        "predownload {}: {} → {} ({} file(s), {:.1} GiB)",
                        part.name,
                        part.from,
                        part.to,
                        part_plan.patch_groups.len() + part_plan.full_files.len(),
                        part_plan.total_bytes as f64 / (1u64 << 30) as f64
                    )))
                    .await;
            }
            planned.push((part, patch_cfg, stage, part_plan));
        }
        if let Some(tx) = &tx {
            let total: u64 = planned.iter().map(|(_, _, _, p)| p.total_bytes).sum();
            let _ = tx.send(crate::ProgressEvent::SetTotal { bytes: total }).await;
        }
        for (part, patch_cfg, stage, part_plan) in planned {
            let download_tx = match &tx {
                Some(tx) => tx.clone(),
                None => {
                    let (tx, _rx) = tokio::sync::mpsc::channel(64);
                    tx
                }
            };
            self.predownload_index(&part_plan, &part.index, &patch_cfg, bases, &stage, download_tx)
                .await?;
            report.parts.push(PackUpdatePlanPart {
                name: part.name.clone(),
                from_version: part.from.clone(),
                to_version: part.to.clone(),
                files: part_plan.patch_groups.len() + part_plan.full_files.len(),
                bytes: part_plan.total_bytes,
            });
        }
        plan_file.ready = true;
        state::write_atomic(&plan_path, &serde_json::to_string_pretty(&plan_file)?)?;
        Ok(report)
    }

    /// Apply a completed pack predownload. `common` first, then the active
    /// tier. The version and tier records are written only after both land.
    pub async fn pack_apply(
        &self,
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackUpdateReport> {
        let path = pack_plan_path(&self.game_folder);
        let data = std::fs::read_to_string(&path)
            .map_err(|_| Error::Patch("no downloaded pack update — run the predownload first".into()))?;
        let plan: PackPlanFile = serde_json::from_str(&data)?;
        if plan.schema != 1 {
            return Err(Error::Patch("unsupported pack update plan".into()));
        }
        if plan.predownload {
            if let Some(tx) = &tx {
                let _ = tx
                    .send(crate::ProgressEvent::Log(
                        "this is a preinstall — the version is not formal yet; apply after it opens".into(),
                    ))
                    .await;
            }
        }
        let local = self
            .local_version()?
            .ok_or_else(|| Error::NoLocalConfig(self.game_folder.clone()))?;
        let tier = Quality::parse(&plan.tier)
            .ok_or_else(|| Error::Patch(format!("unknown tier `{}` in the pack plan", plan.tier)))?;
        let current_tier = installed_tier_version(&self.game_folder, tier, &local)?;

        for part in &plan.parts {
            let current = if part.name == "common" { &local } else { &current_tier };
            if current != &part.from && current != &part.to {
                return Err(Error::Patch(format!(
                    "{} changed from {} to {} since the predownload — download it again",
                    part.name, part.from, current
                )));
            }
        }

        let base_refs: Vec<&str> = plan.bases.iter().map(String::as_str).collect();
        let mut report = PackUpdateReport {
            version: local.clone(),
            predownload: plan.predownload,
            parts: Vec::new(),
        };
        for part in &plan.parts {
            let current = if part.name == "common" { &local } else { &current_tier };
            if current == &part.to || part_already_target(&self.game_folder, &part.index) {
                continue; // already applied, finish the records below
            }
            let stage = pack_staging_dir(&self.game_folder, &part.name);
            let patch_cfg = kuro_api::PatchConfig {
                version: part.from.clone(),
                index_file: part.patch.index_file.clone(),
                base_url: part.patch.base_url.clone(),
            };
            if let Some(tx) = &tx {
                let _ = tx
                    .send(crate::ProgressEvent::Log(format!(
                        "applying {}: {} → {}",
                        part.name, part.from, part.to
                    )))
                    .await;
            }
            let applied = self
                .apply_pack_index(&part.index, &base_refs, &patch_cfg, &part.to, &stage, false, tx.clone())
                .await?;
            report.parts.push(PackUpdatePart {
                name: part.name.clone(),
                from_version: part.from.clone(),
                to_version: part.to.clone(),
                report: applied,
            });
        }

        let common_target = plan
            .parts
            .iter()
            .find(|p| p.name == "common")
            .map(|p| p.to.clone())
            .unwrap_or_else(|| local.clone());
        let tier_target = plan
            .parts
            .iter()
            .find(|p| p.name == tier.pack_key())
            .map(|p| p.to.clone())
            .unwrap_or_else(|| current_tier.clone());

        let cfg = state::read_local_config(&self.game_folder)?
            .unwrap_or_else(|| kuro_api::LocalConfig::new(common_target.clone(), self.server_entry().app_id))
            .retargeted(common_target.clone(), self.server_entry().app_id);
        state::write_local_config(&self.game_folder, &cfg)?;
        register(&self.game_folder, tier, &tier_target, Some(&plan.bundle), None)?;
        let _ = std::fs::remove_dir_all(state::incremental_dir(&self.game_folder).join("packs"));
        report.version = common_target;
        Ok(report)
    }

    /// Delete an installed tier's body and launcher records.
    ///
    /// The tier directory is removed whole, matching the official
    /// `resource --remove`. Optional packs under `Client/Saved/Resources` are
    /// left alone; the body is the bulk of the footprint.
    pub async fn remove_pack(&self, tier: Quality) -> Result<PackRemoveReport> {
        if !self.game.uses_quality_tiers() {
            return Err(Error::Unsupported(
                "this game has no quality packs — only Wuthering Waves has Client/Content/<TIER>",
            ));
        }
        let dir = quality::content_dir(&self.game_folder).join(tier.dir_name());
        let (files, bytes) = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || tree_totals(&dir)
        })
        .await
        .map_err(|e| Error::Patch(format!("remove walk: {e}")))?;
        tokio::task::spawn_blocking(move || match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(&dir),
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&dir),
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| Error::Patch(format!("remove join: {e}")))??;

        unregister(&self.game_folder, tier)?;
        let mut record = read_record(&self.game_folder);
        record.installed.remove(&tier.pack_key());
        record.files.remove(&tier.pack_key());
        write_record(&self.game_folder, &record)?;
        if quality::selected(&self.game_folder) == Some(tier) {
            let _ = std::fs::remove_file(state::quality_file(&self.game_folder));
        }
        Ok(PackRemoveReport { tier, files, bytes })
    }

    /// Plan a pack update without downloading or writing anything.
    pub async fn pack_update_plan(&self, tier: Quality) -> Result<PackUpdatePlan> {
        if !self.game.uses_quality_tiers() {
            return Err(Error::Unsupported(
                "this game has no quality packs — only Wuthering Waves has Client/Content/<TIER>",
            ));
        }
        let config = self.pack_config().await?;
        let bases: Vec<String> = ApiClient::pack_cdn_candidates(&config)?
            .iter()
            .map(|n| n.url.clone())
            .collect();
        self.pack_update_plan_inner(tier, &config, &bases).await
    }

    /// [`GameManager::pack_update_plan`] with an injected document and CDN.
    pub async fn pack_update_plan_inner(
        &self,
        tier: Quality,
        config: &PackConfig,
        bases: &[String],
    ) -> Result<PackUpdatePlan> {
        if bases.is_empty() {
            return Err(Error::NoCdnNode);
        }
        let local = self
            .local_version()?
            .ok_or_else(|| Error::NoLocalConfig(self.game_folder.clone()))?;
        let current_tier = installed_tier_version(&self.game_folder, tier, &local)?;
        let (effective, is_predownload) = effective_pack_config(config, tier, &local, &current_tier)?;
        let work = pack_update_steps(tier, effective, &local, &current_tier)?;
        let common_target = work
            .iter()
            .find(|p| p.name == "common")
            .map(|p| p.to.clone())
            .unwrap_or_else(|| local.clone());
        let mut parts = Vec::new();
        for part in &work {
            let base_refs: Vec<&str> = bases.iter().map(String::as_str).collect();
            let index = self
                .api_client()
                .fetch_manifest_verified(&base_refs, &part.patch.index_file, &part.patch.index_file_md5)
                .await?;
            validate_pack_update_index(&part.name, &part.to, &index)?;
            let stage = pack_staging_dir(&self.game_folder, &part.name);
            let plan = self.plan_from_patch_index_in(&index, &part.from, &part.to, &stage);
            parts.push(PackUpdatePlanPart {
                name: part.name.clone(),
                from_version: part.from.clone(),
                to_version: part.to.clone(),
                files: plan.patch_groups.len() + plan.full_files.len(),
                bytes: plan.total_bytes,
            });
        }
        Ok(PackUpdatePlan {
            version: common_target,
            predownload: is_predownload,
            parts,
        })
    }

    /// Resolve and validate the tier's own pack, without writing anything.
    ///
    /// `common` is the base client and belongs to the game-version path, not to
    /// a tier install. The official launcher's `resource` operation installs
    /// only `resourcePacks[<tier>]`; the `bundles` list is just the record it
    /// writes afterwards.
    async fn plan_pack_tier(
        &self,
        tier: Quality,
        config: &PackConfig,
        bases: &[String],
        tx: Option<&tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<Vec<PlannedPack>> {
        let packs = vec![tier.pack_key()];
        if let Some(tx) = tx {
            let _ = tx
                .send(crate::ProgressEvent::Log(format!(
                    "quality pack {}: {} ({})",
                    tier.as_arg(),
                    packs.join(" + "),
                    bases[0].trim_end_matches('/')
                )))
                .await;
        }

        let mut plan = Vec::with_capacity(packs.len());
        for name in packs {
            let Some(entry) = config.resource_packs.get(&name) else {
                return Err(Error::Patch(format!(
                    "the pack config for this channel has no `{name}` pack (has: {})",
                    config.resource_packs.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            };
            let manifest = self.fetch_pack_manifest(&name, entry, bases).await?;
            if name.eq_ignore_ascii_case(tier.as_arg()) && !manifest_is_authoritative(&manifest) {
                return Err(Error::Patch(format!(
                    "the pack document for `{name}` lists no files — refusing to install {tier_arg} \
                     (the document is stale or was not published yet)",
                    tier_arg = tier.as_arg()
                )));
            }
            plan.push(PlannedPack {
                name,
                entry: entry.clone(),
                manifest,
            });
        }
        Ok(plan)
    }

    /// Fetch one pack's manifest and check it may be installed.
    async fn fetch_pack_manifest(&self, name: &str, entry: &PackEntry, bases: &[String]) -> Result<PatchIndex> {
        if entry.index_file.is_empty() {
            return Err(Error::MissingField("indexFile"));
        }
        if entry.base_url.is_empty() {
            return Err(Error::MissingField("baseUrl"));
        }
        let base_refs: Vec<&str> = bases.iter().map(|s| s.as_str()).collect();
        let manifest = self
            .api_client()
            .fetch_manifest_verified(&base_refs, &entry.index_file, &entry.index_file_md5)
            .await?;
        validate_pack_manifest_for(name, &entry.version, &manifest)?;
        Ok(manifest)
    }

    /// Hash-check one pack and report what is missing, without downloading.
    ///
    /// Splitting this from the download lets a run decide whether it has to
    /// touch anything at all before it withdraws the launcher's records: a
    /// no-op rerun must not open a window in which a perfectly good install
    /// looks unregistered.
    async fn scan_pack_part(
        &self,
        name: &str,
        entry: &PackEntry,
        manifest: &PatchIndex,
        cache: &Md5Cache,
        tx: Option<&tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackScan> {
        let mut todo = Vec::new();
        let skipped = 0usize;
        let mut total = 0u64;
        let mut pending = Vec::new();
        for item in &manifest.resource {
            if item.size == 0 {
                // A zero-length entry is a marker, not a download — but it is
                // still work, so it belongs in `todo`: creating it during the
                // scan made `--check` write to the install.
                let path = self.game_folder.join(normalise(&item.dest));
                let present = std::fs::metadata(&path).is_ok_and(|m| m.len() == 0);
                if !present {
                    todo.push(Missing {
                        item: item.clone(),
                        key: path_key(&item.dest),
                        path,
                        marker: true,
                    });
                }
                continue;
            }
            let key = path_key(&item.dest);
            total += item.size;
            // `key` is the case-folded comparison form; the file itself keeps
            // the manifest's spelling. Writing to the folded path would create
            // `client/content/hd` on case-sensitive hosts and leave the real
            // `Client/Content/HD` empty.
            pending.push((item.clone(), key, self.game_folder.join(normalise(&item.dest))));
        }

        // Hashing is CPU- and I/O-bound and runs for minutes on a full tier;
        // doing it inline would occupy an async worker for the whole scan (the
        // channel verify and merge paths already use a pool for this reason).
        let game_cache = cache.clone();
        let broken = tokio::task::spawn_blocking(move || {
            use rayon::prelude::*;
            pending
                .into_par_iter()
                .filter(|(item, key, path)| {
                    let ok = match std::fs::metadata(path) {
                        Ok(m) if m.len() == item.size => game_cache.matches(key, path, &m, &item.md5),
                        _ => false,
                    };
                    !ok
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| Error::Patch(format!("pack verify task: {e}")))?;
        todo.extend(broken.into_iter().map(|(item, key, path)| Missing {
            item,
            key,
            path,
            marker: false,
        }));

        if let Some(tx) = tx {
            let _ = tx
                .send(crate::ProgressEvent::Log(format!(
                    "{name} {}: {} files, {:.1} GiB{}",
                    entry.version,
                    manifest.resource.len(),
                    total as f64 / (1u64 << 30) as f64,
                    if skipped > 0 {
                        format!(" ({skipped} files this channel describes differently, left to it)")
                    } else {
                        String::new()
                    }
                )))
                .await;
            let _ = tx.send(crate::ProgressEvent::SetTotal { bytes: total }).await;
        }

        Ok(PackScan {
            report: PackPartReport {
                name: name.to_string(),
                version: entry.version.clone(),
                files: manifest.resource.len(),
                downloaded: 0,
                bytes: 0,
                skipped,
            },
            todo,
        })
    }

    /// Download and verify everything [`scan_pack_part`] reported missing.
    async fn write_pack_part(
        &self,
        scan: PackScan,
        entry: &PackEntry,
        bases: &[String],
        cache: &Md5Cache,
        tx: Option<&tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PartOutcome> {
        let PackScan { mut report, todo } = scan;
        let name = report.name.clone();
        let mut broken = Vec::new();
        if todo.is_empty() {
            return Ok(PartOutcome { report, broken });
        }

        let sem = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
        let done = Arc::new(AtomicU64::new(0));
        let mut handles = tokio::task::JoinSet::new();
        for missing in todo {
            let sem = sem.clone();
            let done = done.clone();
            let cache = cache.clone();
            let tx = tx.cloned();
            let fetch = Fetch {
                http: self.http_client(),
                budget: self.download_budget(),
                urls: pack_urls(bases, &entry.base_url, &missing.item.dest),
            };
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let size = if missing.marker {
                    create_empty_marker(&missing.path)?;
                    0
                } else {
                    fetch.file(&missing, tx.as_ref()).await?
                };
                if !missing.item.md5.is_empty() {
                    cache.record_file(&missing.key, &missing.path, &missing.item.md5);
                }
                done.fetch_add(1, Ordering::Relaxed);
                Ok::<u64, Error>(size)
            });
        }
        while let Some(joined) = handles.join_next().await {
            match joined.map_err(|e| Error::Patch(format!("pack join: {e}")))? {
                Ok(size) => {
                    report.downloaded += 1;
                    report.bytes += size;
                    if let Some(tx) = tx {
                        let _ = tx
                            .send(crate::ProgressEvent::FileProgress {
                                name: name.clone(),
                                bytes: done.load(Ordering::Relaxed),
                                total: report.files as u64,
                            })
                            .await;
                    }
                }
                Err(e) => broken.push(e.to_string()),
            }
        }
        // The cache is persisted per part, by the caller; `save` snapshots
        // `touched`, so saving here as well would be redundant but harmless.
        Ok(PartOutcome { report, broken })
    }
}

/// One file's download context: the client and CDN budget are shared, the URL
/// list is per-file (each base + the pack's root + the dest).
struct Fetch {
    http: reqwest::Client,
    budget: crate::download::Budget,
    urls: Vec<String>,
}

impl Fetch {
    /// Fetch one missing file to its temp sibling and swap it into place,
    /// returning the size written.
    async fn file(
        &self,
        missing: &Missing,
        tx: Option<&tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<u64> {
        if let Some(dir) = missing.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = tmp_sibling(&missing.path, "pack");
        download_file(
            &self.http,
            &self.urls,
            &tmp,
            missing.item.size,
            &missing.item.chunk_infos,
            &missing.item.md5,
            &missing.item.dest,
            tx,
            &self.budget,
        )
        .await?;
        safe_replace(&tmp, &missing.path)?;
        Ok(missing.item.size)
    }
}

/// Every CDN URL one pack file can be fetched from, in failover order.
fn pack_urls(bases: &[String], base_url: &str, dest: &str) -> Vec<String> {
    bases
        .iter()
        .map(|b| {
            format!(
                "{}/{}/{}",
                b.trim_end_matches('/'),
                base_url.trim_matches('/'),
                dest.trim_start_matches('/')
            )
        })
        .collect()
}

/// A zero-length entry is a marker, not a download: create it if absent.
///
/// Written through a temp sibling and [`safe_replace`] rather than `fs::write`:
/// both `path.exists()` and `fs::write` follow a symlink, so a link left in the
/// tier directory would have kuro create the file at the link's target —
/// outside the install. A rename replaces the link itself.
fn create_empty_marker(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = crate::path::tmp_sibling(path, "marker");
    std::fs::write(&tmp, [])?;
    crate::atomic::safe_replace(&tmp, path)
}

/// One manifest entry that failed verification, with its resolved path.
struct Missing {
    item: kuro_api::ResourceItem,
    key: String,
    path: PathBuf,
    /// A zero-length entry: create the file instead of fetching it.
    marker: bool,
}

/// The result of verifying one pack, before anything is written.
struct PackScan {
    report: PackPartReport,
    todo: Vec<Missing>,
}

struct PartOutcome {
    report: PackPartReport,
    broken: Vec<String>,
}

/// Reject a pack manifest whose entries do not belong to the pack.
///
/// A tier pack may only write inside its own body directory, and no pack may
/// touch the launcher's records or kuro's cache — a manifest that asked to
/// would otherwise be able to rewrite the install's identity or delete the
/// version record it is about to write.
fn validate_pack_manifest_for(name: &str, version: &str, manifest: &PatchIndex) -> Result<()> {
    let check = |dest: &str| -> Result<()> { pack_path_ok(name, version, dest) };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in &manifest.resource {
        check(&item.dest)?;
        // Every downloadable entry needs a digest: without one there is nothing
        // to verify the fetched bytes against, and the file would be written
        // unverified (or skipped, leaving a hole the report still calls
        // healthy). Live pack documents always carry one; an entry that does not
        // means the document is malformed, so the tier is refused rather than
        // installed partially.
        if item.size > 0 && !is_md5(&item.md5) {
            return Err(Error::Patch(format!(
                "pack manifest entry without a valid md5: {} (size {})",
                item.dest, item.size
            )));
        }
        // Two entries for one path would be queued twice against the same
        // destination (and share one temp path), so whichever writer lands
        // second decides the bytes. Only one of them can be right.
        if !seen.insert(path_key(&item.dest)) {
            return Err(Error::Patch(format!(
                "`{name}` manifest lists {dest} more than once",
                dest = item.dest
            )));
        }
    }
    for dest in &manifest.delete_files {
        check(dest)?;
    }
    if !manifest.delete_files.is_empty() {
        // Not implemented for packs, and silently ignoring one would leave the
        // file behind while the tier still registers as installed. Pruning
        // covers a *tier* body, but not `common`'s files.
        return Err(Error::Unsupported(
            "this pack manifest deletes files, which kuro cannot apply yet — \
             the pack was not installed",
        ));
    }
    for group in &manifest.group_infos {
        check(&group.dest)?;
    }
    if !manifest.group_infos.is_empty() {
        // Pack manifests are full-file lists (verified: no groupInfos, no
        // deleteFiles). Krpdiff groups would need the merge engine and its own
        // source-version bookkeeping, which does not exist for packs — saying
        // so is better than silently downloading the fallbacks and leaving the
        // groups unmerged.
        return Err(Error::Unsupported(
            "this pack manifest uses krpdiff groups, which kuro cannot merge yet — \
             the pack was not installed",
        ));
    }
    Ok(())
}

/// The one path rule the official launcher applies to a pack.
///
/// A tier owns `Client/Content/<TIER>/` plus the target version's resource
/// directory, `Client/Saved/Resources/<version>/`. `common` owns everything
/// outside a tier body, including that resource directory. There is no
/// per-optional-pack shape: since 3.7/3.8 the launcher ships any such file
/// under the version directory, and the version is the only discriminator.
/// Management records stay off limits for every pack.
fn pack_path_ok(name: &str, version: &str, dest: &str) -> Result<()> {
    let key = path_key(dest);
    let first = key.split('/').next().unwrap_or("");
    if first == ".kuro_cache" || first == ".incremental_download" || key.starts_with("launcherdownloadconfig") {
        return Err(Error::UnsafePath(dest.to_string()));
    }
    let normalised = normalise(dest);
    if normalised.split('/').any(|p| p == "." || p == "..") {
        return Err(Error::UnsafePath(dest.to_string()));
    }
    if let Some(rest) = normalised.strip_prefix("Client/Saved/Resources/") {
        let mut parts = rest.split('/');
        let found = parts.next().unwrap_or("");
        let file = parts.next().unwrap_or("");
        if found == version && !file.is_empty() {
            return Ok(());
        }
        return Err(Error::Patch(format!(
            "`{name}` resource entry outside the {version} resource directory: {dest}"
        )));
    }
    if name.eq_ignore_ascii_case("common") {
        if Quality::ALL
            .iter()
            .any(|q| key.starts_with(&format!("client/content/{}/", q.pack_key())))
        {
            return Err(Error::Patch(format!(
                "`common` manifest entry inside a quality-tier body: {dest}"
            )));
        }
        return Ok(());
    }
    let Some(tier) = Quality::ALL.into_iter().find(|q| q.as_arg().eq_ignore_ascii_case(name)) else {
        return Err(Error::Patch(format!("unknown pack `{name}`")));
    };
    if !normalised.starts_with(&format!("Client/Content/{}/", tier.dir_name())) {
        return Err(Error::Patch(format!(
            "`{name}` manifest entry outside Client/Content/{}: {dest}",
            tier.dir_name()
        )));
    }
    Ok(())
}

/// Validate a pack *update* index (`patchConfig`).
///
/// The `resource` array also carries the `.krpdiff` payloads themselves, whose
/// `dest` is a staging filename rather than an install path, so only the group
/// sources/targets and the full-file fallbacks go through the path rule.
fn validate_pack_update_index(name: &str, version: &str, index: &PatchIndex) -> Result<()> {
    let mut writes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut write = |dest: &str| -> Result<()> {
        pack_path_ok(name, version, dest)?;
        if !writes.insert(path_key(dest)) {
            return Err(Error::Patch(format!("`{name}` update lists {dest} more than once")));
        }
        Ok(())
    };
    for item in &index.resource {
        if item.size > 0 && !is_md5(&item.md5) {
            return Err(Error::Patch(format!(
                "`{name}` update entry without a valid md5: {} (size {})",
                item.dest, item.size
            )));
        }
        if !is_krpdiff(&item.dest) {
            write(&item.dest)?;
        }
    }
    for group in &index.group_infos {
        for f in &group.src_files {
            pack_path_ok(name, version, &f.dest)?;
        }
        for f in &group.dst_files {
            write(&f.dest)?;
        }
    }
    for dest in &index.delete_files {
        pack_path_ok(name, version, dest)?;
        if writes.contains(&path_key(dest)) {
            return Err(Error::Patch(format!(
                "`{name}` update writes and deletes the same path: {dest}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuro_api::ResourceItem;

    fn manifest(dests: &[&str]) -> PatchIndex {
        PatchIndex {
            resource: dests
                .iter()
                .map(|d| ResourceItem {
                    dest: d.to_string(),
                    md5: "0".repeat(32),
                    size: 1,
                    from_folder: None,
                    chunk_infos: Vec::new(),
                })
                .collect(),
            delete_files: Vec::new(),
            group_infos: Vec::new(),
            apply_types: Vec::new(),
        }
    }

    #[test]
    fn only_dotted_numeric_versions_are_compared() {
        assert!(is_downgrade("3.7.1", "3.7.0"));
        assert!(is_downgrade("3.10.0", "3.9.9"));
        assert!(!is_downgrade("3.7.1", "3.7.1"));
        assert!(!is_downgrade("3.7.1", "3.7.2"));
        // A shorter spelling of the same version is not a downgrade.
        assert!(!is_downgrade("3.7", "3.7.0"));
        assert!(!is_downgrade("3.7.0", "3.7"));
        // Uncomparable versions are left alone rather than refused.
        assert!(!is_downgrade("build-7", "3.7.0"));
        assert!(!is_downgrade("3.7.1", ""));
    }

    #[test]
    fn tier_pack_is_confined_to_its_body() {
        assert!(
            validate_pack_manifest("hd", &manifest(&["Client/Content/HD/pakchunk1-HD-WindowsNoEditor.pak"])).is_ok()
        );
        let err = validate_pack_manifest(
            "hd",
            &manifest(&["Client/Content/UHD/pakchunk1-UHD-WindowsNoEditor.pak"]),
        );
        assert!(err.is_err(), "an HD pack must not write the UHD body");
    }

    /// `common` is the base client; it must not write into a tier body, which is
    /// what the user's quality choice selects between.
    #[test]
    fn common_pack_is_kept_out_of_the_tier_bodies() {
        assert!(validate_pack_manifest(
            "common",
            &manifest(&["Client/Binaries/Win64/Client-Win64-Shipping.exe"])
        )
        .is_ok());
        for body in ["SD", "HD", "UHD"] {
            let dest = format!("Client/Content/{body}/pakchunk0.pak");
            assert!(
                validate_pack_manifest("common", &manifest(&[&dest])).is_err(),
                "`common` was allowed to write {dest}"
            );
        }
    }

    #[test]
    fn no_pack_may_touch_the_launcher_records_or_the_cache() {
        for bad in [
            "launcherDownloadConfig/hd.json",
            "launcherDownloadConfig.json",
            ".kuro_cache/packs.json",
            ".incremental_download/x.pak",
        ] {
            assert!(
                validate_pack_manifest("common", &manifest(&[bad])).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_body_directory_must_be_spelled_as_the_game_reads_it() {
        // Case-insensitive match is not enough: the game reads
        // `Client/Content/HD`, and a lowercase body is a different directory on
        // any case-sensitive host.
        assert!(
            validate_pack_manifest("hd", &manifest(&["client/content/hd/pakchunk1.pak"])).is_err(),
            "a lowercase body directory was accepted"
        );
        // A leading slash and backslashes are the CDN's ways of saying the
        // same relative path, and must still pass.
        assert!(
            validate_pack_manifest("hd", &manifest(&["/Client/Content/HD/a.pak"])).is_ok(),
            "an absolute-spelled dest was refused"
        );
        assert!(
            validate_pack_manifest("hd", &manifest(&["Client\\Content\\HD\\a.pak"])).is_ok(),
            "a backslash-spelled dest was refused"
        );
    }

    #[test]
    fn a_path_listed_twice_is_refused() {
        let once = manifest(&["Client/Content/HD/a.pak"]);
        assert!(validate_pack_manifest("hd", &once).is_ok());
        assert!(
            validate_pack_manifest("hd", &manifest(&["Client/Content/HD/a.pak", "Client/Content/HD/a.pak"])).is_err(),
            "a duplicate dest was accepted"
        );
    }

    #[test]
    fn a_delete_list_is_refused_rather_than_ignored() {
        let mut m = manifest(&["Client/Content/HD/a.pak"]);
        m.delete_files.push("Client/Content/HD/old.pak".to_string());
        let err = validate_pack_manifest("hd", &m).expect_err("a deleteFiles entry was ignored");
        assert!(
            matches!(err, Error::Unsupported(_)),
            "a language gap should be Unsupported, got {err:?}"
        );
    }

    #[test]
    fn common_is_not_confined_to_one_body() {
        // `common` is the base client: binaries, engine, base paks.
        assert!(validate_pack_manifest(
            "common",
            &manifest(&[
                "Client/Binaries/Win64/Client-Win64-Shipping.exe",
                "Engine/Binaries/ThirdParty/x.dll",
                "Client/Content/Paks/pakchunk0.pak",
            ])
        )
        .is_ok());
    }

    /// The official rule: a tier may write its body or *any* path under the
    /// target version's resource directory. No option-pack name or tier suffix
    /// participates.
    #[test]
    fn a_tier_may_ship_any_path_under_the_target_resource_version() {
        for tier in ["sd", "hd", "uhd"] {
            for dest in [
                "Client/Saved/Resources/3.8.0/Option_Pack_Map_HD/Base/pakchunk50.pak",
                "Client/Saved/Resources/3.8.0/Option_Pack_Map/Base/x.pak",
                "Client/Saved/Resources/3.8.0/anything/at/all.pak",
            ] {
                assert!(
                    validate_pack_manifest(tier, &manifest(&[dest])).is_ok(),
                    "{tier} rejected {dest}"
                );
            }
        }
        assert!(validate_pack_manifest("hd", &manifest(&["Client/Content/UHD/x.pak"])).is_err());
        assert!(validate_pack_manifest("hd", &manifest(&["Client/Binaries/Win64/x.dll"])).is_err());
    }

    #[test]
    fn common_may_ship_the_target_resource_version_too() {
        assert!(validate_pack_manifest(
            "common",
            &manifest(&[
                "Client/Saved/Resources/3.8.0/Option_Pack_Map_HD/Base/x.pak",
                "Client/Binaries/Win64/Client-Win64-Shipping.exe",
            ])
        )
        .is_ok());
        assert!(validate_pack_manifest("common", &manifest(&["Client/Saved/Resources/3.7.1/x.pak"])).is_err());
        assert!(validate_pack_manifest("common", &manifest(&["Client/Content/HD/x.pak"])).is_err());
    }

    #[test]
    fn the_resource_version_is_the_only_discriminator() {
        for bad in [
            "Client/Saved/Config/GameUserSettings.ini",
            "Client/Saved/LocalStorage/x.txt",
            "Client/Saved/Resources/3.7.1/Option_Pack_Map_HD/Base/x.pak",
            "Client/Saved/Resources/latest/Option_Pack_Map_HD/Base/x.pak",
            "Client/Saved/Resources/3.8.0/../../x.pak",
            "Client/Saved/Resources/3.8.0",
        ] {
            assert!(
                validate_pack_manifest("hd", &manifest(&[bad])).is_err(),
                "{bad} was accepted"
            );
        }
        // A leading slash and backslashes are the CDN's spelling of the same
        // relative path, exactly as for the body.
        assert!(validate_pack_manifest("hd", &manifest(&["/Client\\Saved\\Resources\\3.8.0\\x.pak"])).is_ok());
    }

    /// The tests above exercise the live 3.8.0 document. Production passes the
    /// entry's own version.
    fn validate_pack_manifest(name: &str, manifest: &PatchIndex) -> Result<()> {
        validate_pack_manifest_for(name, "3.8.0", manifest)
    }
}
