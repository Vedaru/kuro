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
//! * `common` is reconciled against the *channel* manifest rather than
//!   validated against a separately downloaded target index: the channel is the
//!   authority for its own files, so an entry the channel serves differently is
//!   left to `sync` instead of aborting the tier. That is what keeps the shared
//!   CN pack document usable from Bilibili (747/816 `common` entries agree;
//!   the 69 that differ are channel SDK binaries).
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
//! # The one thing `ww-manager` has that this does not
//!
//! `ww-manager` drives pack *updates* through the incremental engine: it reads
//! `resourcePacks.<name>.patchConfig`, applies the krpdiff groups for
//! `from -> to`, and writes a resumable plan so `--apply` can continue after an
//! interruption. kuro's pack path installs a *tier* from its full manifest, and
//! treats a newer channel version as "verify and fetch what differs". For the
//! packs published today that is the same work: none of the four manifests
//! carries `groupInfos`, and every `patchConfig` entry we probed resolves to a
//! full-file list (the 3.6.0/3.6.1 `common` patches are the only ones with
//! groups, and they are for older palettes). Adding the merge engine here before
//! a pack actually ships groups would be code no run exercises.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kuro_api::{ApiClient, Error, PackConfig, PackEntry, PatchIndex, Result, ServerEntry};

use crate::atomic::safe_replace;
use crate::download::{download_chunked, download_single};
use crate::md5_cache::Md5Cache;
use crate::quality::Quality;
use crate::state;

/// Concurrent files within one pack (the shared [`Budget`] caps total requests).
const FILE_CONCURRENCY: usize = 8;
/// Ranges in flight for one large file.
const CHUNK_CONCURRENCY: usize = 8;
/// Files above this size are fetched as parallel ranges. The pack manifests
/// carry no `chunkInfos`, and pack paks are ~400 MiB each, so this is what stops
/// a tier download from being 100 single-stream transfers.
const SYNTH_CHUNK_SIZE: u64 = 4 * 1024 * 1024;

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

impl PackPartReport {
    pub fn is_complete(&self) -> bool {
        self.downloaded == 0
    }
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
    let path = record_path(game_folder);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(record)?)?;
    Ok(())
}

/// Read the launcher's records for one tier.
pub fn state_of(game_folder: &Path, tier: Quality) -> PackState {
    let marker = std::fs::read_to_string(
        game_folder
            .join(PACKS_DIR)
            .join(format!("{}.json", tier.as_arg().to_ascii_lowercase())),
    )
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
        recorded_version: read_record(game_folder)
            .installed
            .get(&tier.as_arg().to_ascii_lowercase())
            .cloned(),
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
    let mut cfg = state::read_local_config(game_folder)?
        .ok_or_else(|| Error::NoLocalConfig(game_folder.to_path_buf()))?;

    let packs: Vec<String> = bundle_packs
        .map(|p| p.to_vec())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| vec!["common".to_string(), tier.as_arg().to_ascii_lowercase()]);
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
        pack_name: tier.as_arg().to_ascii_lowercase(),
        version: version.to_string(),
    };
    let marker_path = dir.join(format!("{}.json", tier.as_arg().to_ascii_lowercase()));
    state::write_atomic(&marker_path, &serde_json::to_string_pretty(&marker)?)?;

    let mut record = read_record(game_folder);
    let key = tier.as_arg().to_ascii_lowercase();
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
    let marker = game_folder
        .join(PACKS_DIR)
        .join(format!("{}.json", tier.as_arg().to_ascii_lowercase()));
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
fn prune_stale(
    game_folder: &Path,
    tier: Quality,
    keep: &std::collections::HashSet<String>,
) -> usize {
    let key = tier.as_arg().to_ascii_lowercase();
    let mut record = read_record(game_folder);
    let Some(previous) = record.files.get(&key).cloned() else {
        return 0;
    };
    let mut removed = 0;
    let mut parents = std::collections::BTreeSet::new();
    for dest in &previous {
        if keep.contains(&normalise(dest)) {
            continue;
        }
        let path = game_folder.join(relative(dest));
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

/// The manifest of the tier's own pack, not `common`.
///
/// `common` overlaps the channel manifest, which `sync` owns, so its paths must
/// never become this module's pruning candidates or its file record.
fn tier_manifest<'a>(
    plan: &'a [(&str, &PackEntry, PatchIndex)],
    tier: Quality,
) -> Option<&'a PatchIndex> {
    plan.iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(tier.as_arg()))
        .map(|(_, _, m)| m)
}

/// Drop files of the previous install that the tier's manifest no longer lists,
/// and return the dests it does list — the record the next run prunes against.
fn prune_and_record(
    game_folder: &Path,
    tier: Quality,
    plan: &[(&str, &PackEntry, PatchIndex)],
) -> (usize, Vec<String>) {
    let Some(manifest) = tier_manifest(plan, tier) else {
        return (0, Vec::new());
    };
    let keep: std::collections::HashSet<String> = manifest
        .resource
        .iter()
        .map(|r| normalise(&r.dest))
        .collect();
    let pruned = prune_stale(game_folder, tier, &keep);
    let dests = manifest.resource.iter().map(|r| r.dest.clone()).collect();
    (pruned, dests)
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
        self.api_client()
            .fetch_pack_config(entry.pack_config_urls)
            .await
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
            return Err(Error::Unimplemented(
                "this game has no quality packs — only Wuthering Waves has Client/Content/<TIER>",
            ));
        }
        let config = self.pack_config().await?;
        let bases: Vec<String> = ApiClient::pack_cdn_candidates(&config)?
            .iter()
            .map(|n| n.url.clone())
            .collect();
        let channel = self.channel_manifest().await?;
        self.sync_pack_inner(tier, &config, &bases, &channel, repair, tx)
            .await
    }

    /// [`GameManager::sync_pack`] with the network discovery already done, so
    /// the install path is testable against a local server.
    pub async fn sync_pack_inner(
        &self,
        tier: Quality,
        config: &PackConfig,
        bases: &[String],
        channel: &HashMap<String, (u64, String)>,
        repair: bool,
        tx: Option<tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackReport> {
        if bases.is_empty() {
            return Err(Error::NoCdnNode);
        }
        let mut packs = bundle_packs(config, tier);
        if packs.is_empty() {
            packs = vec!["common".to_string(), tier.as_arg().to_ascii_lowercase()];
        }

        if let Some(tx) = &tx {
            let _ = tx
                .send(crate::ProgressEvent::Log(format!(
                    "quality pack {}: {} ({})",
                    tier.as_arg(),
                    packs.join(" + "),
                    bases[0].trim_end_matches('/')
                )))
                .await;
        }

        // Resolve and validate every pack up front: `common` is downloaded
        // before the tier pack, so a tier manifest that turns out to be
        // unusable must be refused before the first byte of `common` is
        // written. Downloading tens of GiB and only then rejecting the tier is
        // the failure this ordering exists to prevent.
        let mut plan: Vec<(&str, &PackEntry, PatchIndex)> = Vec::with_capacity(packs.len());
        for name in &packs {
            let Some(entry) = config.resource_packs.get(name) else {
                return Err(Error::Patch(format!(
                    "the pack config for this channel has no `{name}` pack (has: {})",
                    config
                        .resource_packs
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            let manifest = self.fetch_pack_manifest(name, entry, bases).await?;
            plan.push((name.as_str(), entry, manifest));
        }

        let mut version = String::new();
        for (name, entry, _) in &plan {
            if name.eq_ignore_ascii_case(tier.as_arg()) {
                version = entry.version.clone();
            }
        }
        if version.is_empty() {
            version = config
                .resource_packs
                .get("common")
                .map(|c| c.version.clone())
                .unwrap_or_default();
        }

        // A channel whose pack document lags what is on disk would otherwise
        // "install" a tier backwards, overwriting newer paks with older ones and
        // recording the old version. Numeric comparison only — a version that
        // is not dotted-numeric is not comparable, so it is left alone rather
        // than guessed at.
        if let Some(installed) = read_record(&self.game_folder)
            .installed
            .get(&tier.as_arg().to_ascii_lowercase())
            .cloned()
        {
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
        let mut scans = Vec::with_capacity(plan.len());
        for (name, entry, manifest) in &plan {
            scans.push(
                self.scan_pack_part(name, entry, manifest, channel, tx.as_ref())
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
            for scan in scans {
                for missing in &scan.todo {
                    report.broken.push(missing.item.dest.clone());
                }
                if let Some(part) = report.parts.iter_mut().find(|p| p.name == scan.report.name) {
                    part.downloaded = scan.todo.len();
                    part.bytes = scan.todo.iter().map(|m| m.item.size).sum();
                }
            }
            return Ok(report);
        }

        if scans.iter().all(|s| s.todo.is_empty()) {
            // Nothing to download: only touch the records when they are missing
            // (an already-complete directory, or a launcher that dropped them).
            // Rewriting unconditionally would replace a registry the launcher
            // may have updated with ours.
            let (pruned, written) = prune_and_record(&self.game_folder, tier, &plan);
            report.pruned = pruned;
            let state = state_of(&self.game_folder, tier);
            let current = state.marker.as_ref().is_some_and(|m| m.version == version);
            if !state.registered() || !current {
                register(
                    &self.game_folder,
                    tier,
                    &version,
                    Some(&packs),
                    Some(&written),
                )?;
            }
            report.registered = true;
            return Ok(report);
        }

        // The tier is about to change on disk, so withdraw its launcher records
        // *before* the first byte: a run that dies halfway must leave "not
        // installed", not a record claiming packs that are now partly missing.
        // That is exactly the state the launcher answers with "repair game".
        unregister(&self.game_folder, tier)?;

        for (idx, scan) in scans.into_iter().enumerate() {
            let entry = plan[idx].1;
            let part = self
                .write_pack_part(scan, entry, bases, tx.as_ref())
                .await?;
            report.broken.extend(part.broken);
            if let Some(slot) = report.parts.iter_mut().find(|p| p.name == part.report.name) {
                *slot = part.report;
            }
        }

        if report.broken.is_empty() {
            let (pruned, written) = prune_and_record(&self.game_folder, tier, &plan);
            report.pruned = pruned;
            register(
                &self.game_folder,
                tier,
                &version,
                Some(&packs),
                Some(&written),
            )?;
            report.registered = true;
        }
        Ok(report)
    }

    /// The channel's full manifest, keyed by normalised (lowercase) dest.
    async fn channel_manifest(&self) -> Result<HashMap<String, (u64, String)>> {
        let index = self
            .api_client()
            .fetch_index(&kuro_api::index_url(self.game, self.server)?)
            .await?;
        let nodes = self.api_client().cdn_candidates(&index)?;
        let bases: Vec<&str> = nodes.iter().map(|n| n.url.as_str()).collect();
        let manifest = self
            .api_client()
            .fetch_manifest(&bases, &index.default.config.index_file)
            .await?;
        Ok(manifest
            .resource
            .into_iter()
            .map(|r| (normalise(&r.dest), (r.size, r.md5)))
            .collect())
    }

    /// Fetch one pack's manifest and check it may be installed.
    async fn fetch_pack_manifest(
        &self,
        name: &str,
        entry: &PackEntry,
        bases: &[String],
    ) -> Result<PatchIndex> {
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
        validate_pack_manifest(name, &manifest)?;
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
        channel: &HashMap<String, (u64, String)>,
        tx: Option<&tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PackScan> {
        let is_common = name.eq_ignore_ascii_case("common");
        let mut todo = Vec::new();
        let mut skipped = 0usize;
        let cache = Md5Cache::load(&self.game_folder);
        let mut total = 0u64;
        for item in &manifest.resource {
            if item.size == 0 || item.md5.is_empty() {
                // Nothing to verify against and nothing to fetch — a
                // zero-length marker file is created rather than downloaded.
                if item.size == 0 {
                    let path = self.game_folder.join(relative(&item.dest));
                    if !path.exists() {
                        if let Some(dir) = path.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        std::fs::write(&path, [])?;
                    }
                }
                continue;
            }
            total += item.size;
            let key = normalise(&item.dest);
            // `common` describes the CN base client, which the channel also
            // serves; the channel is the authority for its own files. An entry
            // the channel does not describe, or describes differently, is left
            // to the channel: the pack document is shared across channels, and
            // its copy of a channel-specific binary is the wrong channel's.
            // (CN agrees on all 816; Bilibili agrees on 747 and differs on the
            // 69 `krsdk_*`/anti-cheat binaries, plus one CN-only SDK dll.)
            if is_common
                && channel
                    .get(&key)
                    .is_none_or(|(size, md5)| *size != item.size || md5 != &item.md5)
            {
                skipped += 1;
                continue;
            }
            // `key` is the case-folded comparison form; the file itself keeps
            // the manifest's spelling. Writing to the folded path would create
            // `client/content/hd` on case-sensitive hosts and leave the real
            // `Client/Content/HD` empty.
            let path = self.game_folder.join(relative(&item.dest));
            let ok = match std::fs::metadata(&path) {
                Ok(m) if m.len() == item.size => cache.matches(&key, &path, &m, &item.md5),
                _ => false,
            };
            if !ok {
                todo.push(Missing {
                    item: item.clone(),
                    key,
                    path,
                });
            }
        }
        let _ = cache.save();

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
            let _ = tx
                .send(crate::ProgressEvent::SetTotal { bytes: total })
                .await;
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
            cache,
        })
    }

    /// Download and verify everything [`scan_pack_part`] reported missing.
    #[allow(clippy::too_many_arguments)]
    async fn write_pack_part(
        &self,
        scan: PackScan,
        entry: &PackEntry,
        bases: &[String],
        tx: Option<&tokio::sync::mpsc::Sender<crate::ProgressEvent>>,
    ) -> Result<PartOutcome> {
        let PackScan {
            mut report,
            todo,
            cache,
        } = scan;
        let name = report.name.clone();
        let mut broken = Vec::new();
        if todo.is_empty() {
            return Ok(PartOutcome { report, broken });
        }

        let sem = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
        let done = Arc::new(AtomicU64::new(0));
        let mut handles = tokio::task::JoinSet::new();
        for Missing { item, key, path } in todo {
            let sem = sem.clone();
            let http = self.http_client();
            let budget = self.download_budget();
            let bases = bases.to_vec();
            let cache = cache.clone();
            let base_url = entry.base_url.clone();
            let tx = tx.cloned();
            let done = done.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let urls: Vec<String> = bases
                    .iter()
                    .map(|b| {
                        format!(
                            "{}/{}/{}",
                            b.trim_end_matches('/'),
                            base_url.trim_matches('/'),
                            item.dest.trim_start_matches('/')
                        )
                    })
                    .collect();
                let tmp = with_suffix(&path, ".pack.tmp");
                let chunks = crate::download::chunk_plan(item.size, SYNTH_CHUNK_SIZE);
                if chunks.is_empty() {
                    download_single(
                        &http,
                        &urls,
                        &tmp,
                        Some(item.size),
                        Some(item.md5.as_str()),
                        &item.dest,
                        tx.as_ref(),
                        &budget,
                    )
                    .await?;
                } else {
                    download_chunked(
                        &http,
                        &urls,
                        &tmp,
                        &chunks,
                        Some(item.md5.as_str()),
                        CHUNK_CONCURRENCY,
                        &item.dest,
                        tx.as_ref(),
                        &budget,
                    )
                    .await?;
                }
                safe_replace(&tmp, &path)?;
                cache.record_file(&key, &path, &item.md5);
                done.fetch_add(1, Ordering::Relaxed);
                Ok::<u64, Error>(item.size)
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
        // Persist what was just downloaded, so a rerun after an interruption
        // re-hashes only the tail.
        let _ = cache.save();
        Ok(PartOutcome { report, broken })
    }
}

/// One manifest entry that failed verification, with its resolved path.
struct Missing {
    item: kuro_api::ResourceItem,
    key: String,
    path: PathBuf,
}

/// The result of verifying one pack, before anything is written.
struct PackScan {
    report: PackPartReport,
    todo: Vec<Missing>,
    cache: Md5Cache,
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
fn validate_pack_manifest(name: &str, manifest: &PatchIndex) -> Result<()> {
    let tier = Quality::ALL
        .into_iter()
        .find(|q| q.as_arg().eq_ignore_ascii_case(name));
    let prefix = tier.map(|t| format!("client/content/{}/", t.dir_name().to_ascii_lowercase()));
    let check = |dest: &str| -> Result<()> {
        let key = normalise(dest);
        let first = key.split('/').next().unwrap_or("");
        if first == ".kuro_cache"
            || first == ".incremental_download"
            || key.starts_with("launcherdownloadconfig")
        {
            return Err(Error::UnsafePath(dest.to_string()));
        }
        if let Some(prefix) = &prefix {
            if !key.starts_with(prefix.as_str()) {
                return Err(Error::Patch(format!(
                    "`{name}` manifest entry outside Client/Content/{}: {dest}",
                    tier.map(|t| t.dir_name()).unwrap_or_default()
                )));
            }
        }
        Ok(())
    };
    for item in &manifest.resource {
        check(&item.dest)?;
    }
    for dest in &manifest.delete_files {
        check(dest)?;
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
        return Err(Error::Unimplemented(
            "this pack manifest uses krpdiff groups, which kuro cannot merge yet — \
             the pack was not installed",
        ));
    }
    Ok(())
}

/// The manifest's spelling of a dest, as a path relative to the game folder.
fn relative(dest: &str) -> String {
    dest.trim_start_matches('/').replace('\\', "/")
}

/// Case-folded form, for comparing two manifests' view of the same file.
fn normalise(dest: &str) -> String {
    relative(dest).to_ascii_lowercase()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
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
        assert!(validate_pack_manifest(
            "hd",
            &manifest(&["Client/Content/HD/pakchunk1-HD-WindowsNoEditor.pak"])
        )
        .is_ok());
        let err = validate_pack_manifest(
            "hd",
            &manifest(&["Client/Content/UHD/pakchunk1-UHD-WindowsNoEditor.pak"]),
        );
        assert!(err.is_err(), "an HD pack must not write the UHD body");
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

    #[test]
    fn relative_and_absolute_spellings_are_the_same_entry() {
        assert_eq!(
            normalise("/Client/Content/HD/a.pak"),
            "client/content/hd/a.pak"
        );
        assert_eq!(
            normalise("Client\\Content\\HD\\a.pak"),
            "client/content/hd/a.pak"
        );
    }
}
