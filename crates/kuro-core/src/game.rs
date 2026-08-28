//! `GameManager` — the orchestrator: status / predownload / apply / sync.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kuro_api::config::ServerEntry;
use kuro_api::{
    game_server_by_app_id, index_url, server_entry, ApiClient, ChunkInfo, Error, FileRef, Game,
    GroupInfo, LocalConfig, PatchConfig, PatchIndex, ResourceItem, Server, Result,
};

use crate::atomic::{recover_backup, safe_replace};
use crate::download::{download_chunked, download_single};
use crate::state::{self, incremental_dir};

/// Events emitted during long operations (for the TUI / progress UI).
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Log(String),
    GroupStart { name: String },
    /// Periodic per-file byte progress while downloading.
    FileProgress { name: String, bytes: u64, total: u64 },
    GroupDone { name: String, bytes: u64 },
    /// Total bytes of the operation, known once planning/verification is done.
    SetTotal { bytes: u64 },
    /// Number of items queued for repair/download (replaces a per-item
    /// GroupStart flood for large manifests — one event, not N).
    SetQueued { count: usize },
    Done,
}

/// Summary of one file that still needs downloading.
#[derive(Debug, Clone)]
pub struct PendingGroup {
    pub name: String,
    pub size: u64,
    pub local_ready: bool,
}

/// Result of planning a predownload.
#[derive(Debug, Clone)]
pub struct PredownloadPlan {
    pub from_version: String,
    pub to_version: String,
    pub patch_groups: Vec<PendingGroup>,
    pub full_files: Vec<PendingGroup>,
    pub total_bytes: u64,
}

/// Local vs remote version snapshot.
#[derive(Debug, Clone)]
pub struct GameStatus {
    pub game: Game,
    pub server: Server,
    pub local_version: Option<String>,
    pub remote_version: String,
    pub update_available: bool,
}

const CHUNK_CONCURRENCY: usize = 32;
/// CN 3.6.0 manifests carry no chunkInfos — synthesize fixed-size ranges so
/// large paks download over parallel connections (CDNs rate-limit per conn).
const SYNTH_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// Concurrent FILES in the repair/install download loop (each file then fans
/// out to CHUNK_CONCURRENCY range requests → bounded total connections).
const REPAIR_FILE_CONCURRENCY: usize = 4;
/// Parallel krpdiff merges during apply (CPU-bound, native engine).
const MERGE_CONCURRENCY: usize = 4;

pub struct GameManager {
    pub game_folder: PathBuf,
    pub game: Game,
    pub server: Server,
    api: ApiClient,
    http: reqwest::Client,
}

impl GameManager {
    /// Open a game folder, auto-detecting game + server from
    /// `launcherDownloadConfig.json` (same file the official launcher writes).
    pub async fn open(game_folder: PathBuf) -> Result<Self> {
        let cfg = state::read_local_config(&game_folder)?
            .ok_or_else(|| Error::NoLocalConfig(game_folder.clone()))?;
        let (game, server) =
            game_server_by_app_id(&cfg.app_id).ok_or_else(|| Error::UnknownAppId(cfg.app_id.clone()))?;
        let api = ApiClient::new()?;
        let http = reqwest::Client::builder()
            .user_agent("kuro/0.1 (+https://github.com/vedaru/kuro)")
            .build()?;
        Ok(Self {
            game_folder,
            game,
            server,
            api,
            http,
        })
    }

    pub fn server_entry(&self) -> &'static ServerEntry {
        server_entry(self.game, self.server).expect("registry covers all known servers")
    }

    /// Local version from `launcherDownloadConfig.json`.
    pub fn local_version(&self) -> Result<Option<String>> {
        Ok(state::read_local_config(&self.game_folder)?.map(|c| c.version))
    }

    /// Remote (current) version + whether an update is available.
    pub async fn status(&self) -> Result<GameStatus> {
        let index = self.api.fetch_index(&index_url(self.game, self.server)?).await?;
        let remote = index.default.version.clone();
        let local = self.local_version()?;
        Ok(GameStatus {
            game: self.game,
            server: self.server,
            local_version: local.clone(),
            remote_version: remote.clone(),
            update_available: local.as_deref() != Some(remote.as_str()),
        })
    }

    /// Figure out what a predownload would fetch, without downloading.
    pub async fn plan_predownload(&self) -> Result<PredownloadPlan> {
        let cfg = self
            .local_version()?
            .ok_or_else(|| Error::NoLocalConfig(self.game_folder.clone()))?;
        let from_version = cfg;

        let index = self.api.fetch_index(&index_url(self.game, self.server)?).await?;
        let cdn = self.api.pick_cdn(&index)?.url.clone();
        let to_version = index.default.version.clone();

        // already up to date — nothing to plan
        if from_version == to_version {
            return Ok(PredownloadPlan {
                from_version,
                to_version,
                patch_groups: vec![],
                full_files: vec![],
                total_bytes: 0,
            });
        }

        let patch_cfg = index
            .default
            .config
            .patch_config
            .iter()
            .find(|p| p.version == from_version)
            .ok_or_else(|| Error::MissingField("patchConfig entry for local version"))?;
        let patch_index = self.api.fetch_patch_index(&cdn, patch_cfg).await?;

        let mut patch_groups = Vec::new();
        let mut full_files = Vec::new();
        let mut total = 0u64;

        let res_by_dest: std::collections::HashMap<&str, &ResourceItem> = patch_index
            .resource
            .iter()
            .map(|r| (r.dest.as_str(), r))
            .collect();

        for group in &patch_index.group_infos {
            if group_already_target(&self.game_folder, group) {
                continue;
            }
            let info = res_by_dest.get(group.dest.as_str()).copied();
            let size = info.map(|r| r.size).unwrap_or(0);
            let staged = state::staged_patch_path(&self.game_folder, &group.dest);
            let ready = file_matches(&staged, size, info.map(|r| r.md5.as_str()).unwrap_or(""));
            if !ready {
                total += size;
            }
            patch_groups.push(PendingGroup {
                name: group.dest.clone(),
                size,
                local_ready: ready,
            });
        }

        for item in &patch_index.resource {
            if is_krpdiff(&item.dest) {
                continue; // handled above
            }
            if item.from_folder.as_deref().is_none() {
                continue;
            }
            let staged = state::staged_resource_path(&self.game_folder, &item.dest);
            let ready = file_matches(&staged, item.size, &item.md5);
            if !ready {
                total += item.size;
            }
            full_files.push(PendingGroup {
                name: item.dest.clone(),
                size: item.size,
                local_ready: ready,
            });
        }

        Ok(PredownloadPlan {
            from_version,
            to_version,
            patch_groups,
            full_files,
            total_bytes: total,
        })
    }

    /// Download all pending krpdiffs + full-file fallbacks into the staging
    /// dir. Resumable: already-complete files are skipped. Emits progress
    /// events on `tx` (drop the sender's other clones to see completion).
    pub async fn predownload(
        &self,
        plan: &PredownloadPlan,
        tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    ) -> Result<()> {
        let _ = tx
            .send(ProgressEvent::Log(format!(
                "predownload {} → {} ({} groups, {:.1} GiB)",
                plan.from_version,
                plan.to_version,
                plan.patch_groups.len(),
                plan.total_bytes as f64 / (1 << 30) as f64
            )))
            .await;
        let index = self.api.fetch_index(&index_url(self.game, self.server)?).await?;
        let cdn = self.api.pick_cdn(&index)?.url.clone();

        let dir = incremental_dir(&self.game_folder);
        std::fs::create_dir_all(&dir)?;

        // nothing to download (already up to date)
        if plan.patch_groups.is_empty() && plan.full_files.is_empty() {
            let _ = tx.send(ProgressEvent::Log("already up to date".into())).await;
            let _ = tx.send(ProgressEvent::Done).await;
            return Ok(());
        }

        let patch_cfg = index
            .default
            .config
            .patch_config
            .iter()
            .find(|p| p.version == plan.from_version)
            .ok_or_else(|| Error::MissingField("patchConfig entry for local version"))?;
        let patch_index = self.api.fetch_patch_index(&cdn, patch_cfg).await?;
        let res_by_dest: std::collections::HashMap<&str, &ResourceItem> = patch_index
            .resource
            .iter()
            .map(|r| (r.dest.as_str(), r))
            .collect();

        for group in &plan.patch_groups {
            if group.local_ready {
                continue;
            }
            let _ = tx
                .send(ProgressEvent::GroupStart {
                    name: group.name.clone(),
                })
                .await;
            let _ = tx
                .send(ProgressEvent::FileProgress {
                    name: group.name.clone(),
                    bytes: 0,
                    total: group.size,
                })
                .await;
            let url = ApiClient::krpdiff_url(&cdn, patch_cfg, &group.name);
            let staged = state::staged_patch_path(&self.game_folder, &group.name);
            let tmp = staged.with_extension("krpdiff.tmp");
            download_single(
                &self.http,
                &url,
                &tmp,
                Some(group.size),
                None,
                &group.name,
                Some(&tx),
            )
            .await?;
            std::fs::rename(&tmp, &staged)?;
            let _ = tx
                .send(ProgressEvent::GroupDone {
                    name: group.name.clone(),
                    bytes: group.size,
                })
                .await;
        }

        for item in &plan.full_files {
            if item.local_ready {
                continue;
            }
            let _ = tx
                .send(ProgressEvent::GroupStart {
                    name: item.name.clone(),
                })
                .await;
            let _ = tx
                .send(ProgressEvent::FileProgress {
                    name: item.name.clone(),
                    bytes: 0,
                    total: item.size,
                })
                .await;
            let res = res_by_dest
                .get(item.name.as_str())
                .ok_or_else(|| Error::MissingField("resource entry"))?;
            let from = res.from_folder.as_deref().ok_or(Error::MissingField("fromFolder"))?;
            let url = ApiClient::resource_url(&cdn, from, &res.dest);
            let staged = state::staged_resource_path(&self.game_folder, &res.dest);
            std::fs::create_dir_all(staged.parent().unwrap())?;
            let tmp = staged.with_extension("tmp");
            // CN 3.6.0 manifests carry no chunkInfos; synthesize fixed-size
            // ranges so big paks download over parallel connections (the CDN
            // rate-limits per connection). Whole-file MD5 is still verified.
            let chunk_infos = if res.chunk_infos.is_empty() && res.size > SYNTH_CHUNK_SIZE {
                let n = (res.size + SYNTH_CHUNK_SIZE - 1) / SYNTH_CHUNK_SIZE;
                (0..n)
                    .map(|i| ChunkInfo {
                        start: i * SYNTH_CHUNK_SIZE,
                        end: ((i + 1) * SYNTH_CHUNK_SIZE - 1).min(res.size - 1),
                        md5: String::new(),
                    })
                    .collect::<Vec<_>>()
            } else {
                res.chunk_infos.clone()
            };
            if chunk_infos.is_empty() {
                download_single(
                    &self.http,
                    &url,
                    &tmp,
                    Some(res.size),
                    Some(&res.md5),
                    &res.dest,
                    Some(&tx),
                )
                .await?;
            } else {
                download_chunked(
                    &self.http,
                    &url,
                    &tmp,
                    &chunk_infos,
                    Some(&res.md5),
                    CHUNK_CONCURRENCY,
                    &res.dest,
                    Some(&tx),
                )
                .await?;
            }
            std::fs::rename(&tmp, &staged)?;
            let _ = tx
                .send(ProgressEvent::GroupDone {
                    name: item.name.clone(),
                    bytes: item.size,
                })
                .await;
        }

        let _ = tx.send(ProgressEvent::Done).await;
        Ok(())
    }

    /// Apply a downloaded incremental update: merge krpdiffs natively, verify,
    /// then atomically swap into the game folder. The game must not be running.
    pub async fn apply(&self) -> Result<ApplyReport> {
        let from_version = self
            .local_version()?
            .ok_or_else(|| Error::NoLocalConfig(self.game_folder.clone()))?;

        let index = self.api.fetch_index(&index_url(self.game, self.server)?).await?;
        let remote = index.default.version.clone();
        if remote == from_version {
            return Ok(ApplyReport::default()); // nothing to do
        }
        let cdn = self.api.pick_cdn(&index)?.url.clone();
        let patch_cfg = index
            .default
            .config
            .patch_config
            .iter()
            .find(|p| p.version == from_version)
            .ok_or_else(|| Error::MissingField("patchConfig entry for local version"))?;
        let patch_index = self.api.fetch_patch_index(&cdn, patch_cfg).await?;

        self.apply_inner(&patch_index, &cdn, patch_cfg, &remote).await
    }

    /// The apply pipeline, testable with a synthetic `PatchIndex`.
    ///
    /// 1. merge phase: every krpdiff group -> staged outputs (native KrDiff,
    ///    up to `MERGE_CONCURRENCY` in parallel; fallback = full-file download)
    /// 2. migration phase: verified outputs swapped in atomically (`.bak`)
    /// 3. delete phase: `deleteFiles` removed
    /// 4. local version bumped, staging dir cleaned
    ///
    /// On any failure the game folder is left untouched; staging survives for
    /// a retry.
    pub async fn apply_inner(
        &self,
        patch_index: &PatchIndex,
        cdn: &str,
        patch_cfg: &PatchConfig,
        to_version: &str,
    ) -> Result<ApplyReport> {
        let inc = incremental_dir(&self.game_folder);
        if !inc.exists() {
            return Err(Error::MissingField(
                ".incremental_download — run predownload first",
            ));
        }

        let res_by_dest: HashMap<String, ResourceItem> = patch_index
            .resource
            .iter()
            .cloned()
            .map(|r| (r.dest.clone(), r))
            .collect();
        let complete_dests: HashSet<String> = patch_index
            .resource
            .iter()
            .filter(|r| !is_krpdiff(&r.dest))
            .map(|r| r.dest.clone())
            .collect();

        // ---- merge phase ----
        let mut groups: Vec<GroupInfo> = patch_index.group_infos.clone();
        // biggest groups first (like ww-manager)
        groups.sort_by_key(|g| std::cmp::Reverse(g.dst_files.iter().map(|d| d.size).sum::<u64>()));

        let sem = Arc::new(tokio::sync::Semaphore::new(MERGE_CONCURRENCY));
        let mut handles = Vec::with_capacity(groups.len());
        for (idx, group) in groups.into_iter().enumerate() {
            let sem = sem.clone();
            let game_folder = self.game_folder.clone();
            let inc = inc.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                merge_one_group(&game_folder, &inc, &group, idx).await
            }));
        }

        let mut outcomes: Vec<(String, GroupOutcome)> = Vec::with_capacity(handles.len());
        let mut fallback_dests: Vec<FileRef> = Vec::new();
        for h in handles {
            let (name, outcome) = h
                .await
                .map_err(|e| Error::Patch(format!("merge task join: {e}")))??;
            match &outcome {
                GroupOutcome::Fallback(files) => fallback_dests.extend(files.iter().cloned()),
                GroupOutcome::Merged | GroupOutcome::Skipped => {}
            }
            outcomes.push((name, outcome));
        }

        // ---- fallback downloads (full files, chunked when possible) ----
        let mut seen: HashSet<String> = HashSet::new();
        for dst in fallback_dests {
            if !seen.insert(dst.dest.clone()) {
                continue;
            }
            let res = res_by_dest.get(&dst.dest);
            let (url, chunks, md5) = match res {
                Some(r) if r.from_folder.is_some() => (
                    ApiClient::resource_url(cdn, r.from_folder.as_deref().unwrap(), &dst.dest),
                    r.chunk_infos.clone(),
                    r.md5.clone(),
                ),
                _ => {
                    // fall back to the patch's zip base
                    (ApiClient::resource_url(cdn, &patch_cfg.base_url, &dst.dest), vec![], String::new())
                }
            };
            let staged = state::staged_patch_path(&self.game_folder, &dst.dest);
            std::fs::create_dir_all(staged.parent().unwrap())?;
            let tmp = staged.with_extension("tmp");
            if chunks.is_empty() {
                download_single(
                    &self.http,
                    &url,
                    &tmp,
                    Some(dst.size),
                    Some(&dst.md5),
                    &dst.dest,
                    None,
                )
                .await?;
            } else {
                download_chunked(
                    &self.http,
                    &url,
                    &tmp,
                    &chunks,
                    Some(&md5),
                    CHUNK_CONCURRENCY,
                    &dst.dest,
                    None,
                )
                .await?;
            }
            std::fs::rename(&tmp, &staged)?;
        }

        // ---- migration phase: verify everything is staged, then swap ----
        let mut staged_outputs: Vec<(String, PathBuf)> = Vec::new();
        for group in &patch_index.group_infos {
            for dst in &group.dst_files {
                if complete_dests.contains(&dst.dest) {
                    continue; // handled in the complete-files pass
                }
                let staged = state::staged_patch_path(&self.game_folder, &dst.dest);
                if !staged.exists() {
                    return Err(Error::Patch(format!(
                        "staged output missing for {} — rerun predownload/apply",
                        dst.dest
                    )));
                }
                staged_outputs.push((dst.dest.clone(), staged));
            }
        }

        let mut report = ApplyReport {
            merged: outcomes
                .iter()
                .filter(|(_, o)| matches!(o, GroupOutcome::Merged))
                .count(),
            skipped: outcomes
                .iter()
                .filter(|(_, o)| matches!(o, GroupOutcome::Skipped))
                .count(),
            ..Default::default()
        };

        for (dest, staged) in &staged_outputs {
            let game_path = self.game_folder.join(dest.trim_start_matches('/'));
            recover_backup(&game_path)?;
            safe_replace(staged, &game_path)?;
            report.swapped += 1;
        }

        // complete-file fallbacks (the big ones, e.g. the main exe)
        for item in &patch_index.resource {
            if is_krpdiff(&item.dest) {
                continue;
            }
            let staged = state::staged_resource_path(&self.game_folder, &item.dest);
            if !staged.exists() {
                continue; // not downloaded (already current or not needed)
            }
            let game_path = self.game_folder.join(item.dest.trim_start_matches('/'));
            std::fs::create_dir_all(game_path.parent().unwrap())?;
            recover_backup(&game_path)?;
            safe_replace(&staged, &game_path)?;
            report.swapped += 1;
        }

        // ---- delete phase ----
        for f in &patch_index.delete_files {
            let game_path = self.game_folder.join(f.trim_start_matches('/'));
            if game_path.exists() {
                std::fs::remove_file(&game_path)?;
                report.deleted.push(f.clone());
            }
        }

        // ---- finish: bump version, clean staging ----
        let cfg = LocalConfig {
            version: to_version.to_string(),
            app_id: self.server_entry().app_id.to_string(),
            group: "default".to_string(),
        };
        state::write_local_config(&self.game_folder, &cfg)?;
        std::fs::remove_dir_all(&inc)?;

        Ok(report)
    }

    /// Install a game from zero into `game_folder`: fetch the live manifest,
    /// write `launcherDownloadConfig.json` (remote version + appId), then
    /// sync the full client. Works for any Kuro game in the registry.
    pub async fn install(game: Game, server: Server, game_folder: PathBuf) -> Result<InstallReport> {
        Self::install_with_progress(game, server, game_folder, None).await
    }

    /// Install with progress events (used by the TUI).
    pub async fn install_with_progress(
        game: Game,
        server: Server,
        game_folder: PathBuf,
        tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<InstallReport> {
        let entry = server_entry(game, server)
            .ok_or_else(|| Error::UnknownAppId(format!("{game}/{server}")))?;
        let api = ApiClient::new()?;
        let index = api.fetch_index(&index_url(game, server)?).await?;
        let version = index.default.version.clone();

        std::fs::create_dir_all(&game_folder)?;
        let cfg = LocalConfig {
            version: version.clone(),
            app_id: entry.app_id.to_string(),
            group: "default".to_string(),
        };
        state::write_local_config(&game_folder, &cfg)?;

        let mgr = Self::open(game_folder.clone()).await?;
        let sync = mgr.sync_with_progress(tx).await?;
        let game_exe = find_game_exe(&game_folder);
        Ok(InstallReport {
            version,
            sync,
            game_exe,
        })
    }

    /// Switch server channel by swapping only the channel-specific files and
    /// updating the appId (CN <-> Bilibili). Global is a different package —
    /// not supported for fast-switch (mirrors ww-manager).
    pub async fn checkout(&self, target: Server) -> Result<CheckoutReport> {
        if matches!(target, Server::Global) {
            return Err(Error::Unimplemented(
                "global fast-switch is not supported (package differences) — full sync instead",
            ));
        }
        self.checkout_inner(target, None).await
    }

    /// Checkout core, testable with `api_url` pointed at a local server.
    pub async fn checkout_inner(
        &self,
        target: Server,
        api_url: Option<&str>,
    ) -> Result<CheckoutReport> {
        let entry = server_entry(self.game, target).expect("registry covers all known servers");
        let api_url = match api_url {
            Some(u) => u.to_string(),
            None => index_url(self.game, target)?,
        };

        let index = self.api.fetch_index(&api_url).await?;
        let cdn = self.api.pick_cdn(&index)?.url.clone();
        let cfg = &index.default.config;
        let to_version = cfg.version.clone();
        let base = cfg.base_url.clone();

        // full index → md5s for the diff files
        let full_index: PatchIndex = self
            .http
            .get(format!(
                "{}/{}",
                cdn.trim_end_matches('/'),
                cfg.index_file.trim_start_matches('/')
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let md5_by_dest: HashMap<String, String> = full_index
            .resource
            .iter()
            .map(|r| (r.dest.clone(), r.md5.clone()))
            .collect();

        let mut swapped = 0;
        for f in entry.diff_files {
            let Some(expected_md5) = md5_by_dest.get(*f) else {
                continue;
            };
            let url = ApiClient::resource_url(&cdn, &base, f);
            let game_path = self.game_folder.join(f.trim_start_matches('/'));
            std::fs::create_dir_all(game_path.parent().unwrap())?;
            let tmp = game_path.with_extension("checkout.tmp");
            download_single(
                &self.http,
                &url,
                &tmp,
                None,
                Some(expected_md5),
                f,
                None,
            )
            .await?;
            safe_replace(&tmp, &game_path)?;
            swapped += 1;
        }

        if swapped == 0 {
            return Err(Error::Patch(
                "checkout: no channel files could be swapped (missing from target manifest) — config left unchanged".into(),
            ));
        }

        let cfg = LocalConfig {
            version: to_version.clone(),
            app_id: entry.app_id.to_string(),
            group: "default".to_string(),
        };
        state::write_local_config(&self.game_folder, &cfg)?;

        Ok(CheckoutReport {
            from_server: self.server,
            to_server: target,
            swapped_files: swapped,
            new_version: to_version,
        })
    }
    pub async fn sync(&self) -> Result<SyncReport> {
        self.sync_with_progress(None).await
    }

    /// Sync with optional progress events (used by install and the TUI).
    pub async fn sync_with_progress(
        &self,
        tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<SyncReport> {
        let index = self.api.fetch_index(&index_url(self.game, self.server)?).await?;
        let cdn = self.api.pick_cdn(&index)?.url.clone();
        let cfg = &index.default.config;
        let url = format!(
            "{}/{}",
            cdn.trim_end_matches('/'),
            cfg.index_file.trim_start_matches('/')
        );
        let full_index: PatchIndex = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        self.sync_inner_with_progress(&full_index, &cdn, &cfg.base_url, tx)
            .await
    }

    /// Sync core, testable with a synthetic index + local HTTP server.
    pub async fn sync_inner(
        &self,
        full_index: &PatchIndex,
        cdn: &str,
        base: &str,
    ) -> Result<SyncReport> {
        self.sync_inner_with_progress(full_index, cdn, base, None).await
    }

    /// Sync core with progress events.
    pub async fn sync_inner_with_progress(
        &self,
        full_index: &PatchIndex,
        cdn: &str,
        base: &str,
        tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<SyncReport> {
        let items = full_index.resource.clone();
        let total_files = items.len() as u64;

        if let Some(tx) = &tx {
            let _ = tx
                .send(ProgressEvent::Log(format!("verifying {total_files} files…")))
                .await;
        }

        // verify phase — hash the whole tree in parallel off the async runtime
        let game_folder = self.game_folder.clone();
        let verify_tx = tx.clone();
        let checked = Arc::new(AtomicUsize::new(0));
        let checks: Vec<(ResourceItem, bool)> = tokio::task::spawn_blocking(move || {
            use rayon::prelude::*;
            items
                .par_iter()
                .map(|item| {
                    let n = checked.fetch_add(1, Ordering::SeqCst) + 1;
                    if n.is_multiple_of(256) {
                        if let Some(tx) = &verify_tx {
                            let _ = tx.try_send(ProgressEvent::FileProgress {
                                name: "verify".to_string(),
                                bytes: n as u64,
                                total: total_files,
                            });
                        }
                    }
                    let p = game_folder.join(item.dest.trim_start_matches('/'));
                    let size_ok = std::fs::metadata(&p)
                        .map(|m| m.len() == item.size)
                        .unwrap_or(false);
                    let ok = size_ok
                        && (item.md5.is_empty()
                            || kuro_patch::md5_file(&p)
                                .map(|a| a == item.md5)
                                .unwrap_or(false));
                    (item.clone(), ok)
                })
                .collect()
        })
        .await
        .map_err(|e| Error::Patch(format!("verify task join: {e}")))?;

        let mut report = SyncReport {
            checked: checks.len(),
            ..Default::default()
        };
        let mut to_fix: Vec<ResourceItem> = Vec::new();
        for (item, ok) in checks {
            if ok {
                report.ok += 1;
            } else {
                to_fix.push(item);
            }
        }

        let total: u64 = to_fix.iter().map(|i| i.size).sum();
        if let Some(tx) = &tx {
            if to_fix.is_empty() {
                let _ = tx
                    .send(ProgressEvent::Log(format!(
                        "all {} files ok",
                        report.checked
                    )))
                    .await;
            } else {
                let _ = tx
                    .send(ProgressEvent::Log(format!(
                        "{} files need repair ({:.1} GiB)",
                        to_fix.len(),
                        total as f64 / (1 << 30) as f64
                    )))
                    .await;
            }
            let _ = tx.send(ProgressEvent::SetTotal { bytes: total }).await;
        }

        // repair phase — parallel downloads, verified before swap
        if let Some(tx) = &tx {
            let _ = tx
                .send(ProgressEvent::SetQueued {
                    count: to_fix.len(),
                })
                .await;
        }
        let sem = Arc::new(tokio::sync::Semaphore::new(REPAIR_FILE_CONCURRENCY));
        let mut handles = tokio::task::JoinSet::new();
        for item in to_fix {
            let sem = sem.clone();
            let http = self.http.clone();
            let game_folder = self.game_folder.clone();
            let cdn = cdn.to_string();
            let base = base.to_string();
            let tx = tx.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let from = item.from_folder.clone().unwrap_or_else(|| base.clone());
                let url = ApiClient::resource_url(&cdn, &from, &item.dest);
                let game_path = game_folder.join(item.dest.trim_start_matches('/'));
                std::fs::create_dir_all(game_path.parent().unwrap())?;
                let tmp = game_path.with_extension("sync.tmp");
                // CN 3.6.0 manifests carry no chunkInfos; synthesize fixed-size
                // ranges so big paks download over parallel connections (the
                // CDN rate-limits per connection). Whole-file MD5 still verified.
                let chunk_infos = if item.chunk_infos.is_empty() && item.size > SYNTH_CHUNK_SIZE {
                    let n = (item.size + SYNTH_CHUNK_SIZE - 1) / SYNTH_CHUNK_SIZE;
                    (0..n)
                        .map(|i| ChunkInfo {
                            start: i * SYNTH_CHUNK_SIZE,
                            end: ((i + 1) * SYNTH_CHUNK_SIZE - 1).min(item.size - 1),
                            md5: String::new(),
                        })
                        .collect::<Vec<_>>()
                } else {
                    item.chunk_infos.clone()
                };
                if chunk_infos.is_empty() {
                    download_single(
                        &http,
                        &url,
                        &tmp,
                        Some(item.size),
                        Some(&item.md5),
                        &item.dest,
                        tx.as_ref(),
                    )
                    .await?;
                } else {
                    download_chunked(
                        &http,
                        &url,
                        &tmp,
                        &chunk_infos,
                        Some(&item.md5),
                        CHUNK_CONCURRENCY,
                        &item.dest,
                        tx.as_ref(),
                    )
                    .await?;
                }
                safe_replace(&tmp, &game_path)?;
                Ok::<_, Error>((item.dest, item.size))
            });
        }
        // GroupDone in completion order, not spawn order: a slow first file
        // must not stall progress reporting for every file after it.
        while let Some(joined) = handles.join_next().await {
            let inner: std::result::Result<(String, u64), Error> = joined
                .map_err(|e| Error::Patch(format!("repair join: {e}")))?;
            match inner {
                Ok((dest, size)) => {
                    report.repaired += 1;
                    report.repaired_bytes += size;
                    if let Some(tx) = &tx {
                        let _ = tx
                            .send(ProgressEvent::GroupDone {
                                name: dest,
                                bytes: size,
                            })
                            .await;
                    }
                }
                Err(e) => report.failed.push(e.to_string()),
            }
        }

        // orphan sweep — anything on disk the manifest doesn't list (and
        // isn't part of our own bookkeeping) gets removed. Closes the fourth
        // case the doc-comment doesn't name: "exists on disk, not in manifest".
        report.orphans_removed = sweep_orphans(&self.game_folder, &full_index.resource)?;

        Ok(report)
    }
}

/// Per-group result of the merge phase.
enum GroupOutcome {
    Merged,
    Skipped,
    /// The merge could not be used; these files were (or will be) downloaded
    /// in full instead.
    Fallback(Vec<FileRef>),
}

/// Merge one krpdiff group into staged outputs (or decide a fallback is
/// needed). Reads only; the game folder is not modified.
async fn merge_one_group(
    game_folder: &Path,
    inc: &Path,
    group: &GroupInfo,
    idx: usize,
) -> Result<(String, GroupOutcome)> {
    let name = group.dest.clone();

    // already at target?
    if group_already_target(game_folder, group) {
        return Ok((name, GroupOutcome::Skipped));
    }

    // source sanity check (with .bak recovery)
    let Some(first_src) = group.src_files.first() else {
        return Ok((name, GroupOutcome::Fallback(group.dst_files.clone())));
    };
    let src_path = game_folder.join(first_src.dest.trim_start_matches('/'));
    if !src_path.exists() {
        recover_backup(&src_path)?;
    }
    let local_md5 = match kuro_patch::md5_file(&src_path) {
        Ok(m) => m,
        Err(_) => return Ok((name, GroupOutcome::Fallback(group.dst_files.clone()))),
    };
    let dst_md5s: HashSet<&str> = group.dst_files.iter().map(|d| d.md5.as_str()).collect();
    if local_md5 != first_src.md5 && !dst_md5s.contains(local_md5.as_str()) {
        return Ok((name, GroupOutcome::Fallback(group.dst_files.clone())));
    }

    // merge
    let krpdiff_path = inc.join(&group.dest);
    let out_dir = inc.join(".apply_tmp").join(format!("group_{idx}"));
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)?;
    }
    let merge_res = tokio::task::spawn_blocking({
        let game_folder = game_folder.to_path_buf();
        let krpdiff_path = krpdiff_path.clone();
        let out_dir = out_dir.clone();
        move || kuro_patch::apply_krdiff(&game_folder, &krpdiff_path, &out_dir)
    })
    .await
    .map_err(|e| Error::Patch(format!("merge task join: {e}")))?;

    if let Err(_e) = merge_res {
        let _ = std::fs::remove_dir_all(&out_dir);
        return Ok((name, GroupOutcome::Fallback(group.dst_files.clone())));
    }

    // verify + stage outputs
    let mut missing: Vec<FileRef> = Vec::new();
    for dst in &group.dst_files {
        let out_file = out_dir.join(dst.dest.trim_start_matches('/'));
        let good = match kuro_patch::md5_file(&out_file) {
            Ok(m) => m == dst.md5,
            Err(_) => false,
        };
        if !good {
            missing.push(dst.clone());
            continue;
        }
        // NOTE: `inc` is already the incremental dir — join the relative dest
        // directly (staged_patch_path would append `.incremental_download` again).
        let staged = inc.join(dst.dest.trim_start_matches('/'));
        std::fs::create_dir_all(staged.parent().unwrap())?;
        std::fs::rename(&out_file, &staged)?;
    }
    let _ = std::fs::remove_dir_all(&out_dir);

    if missing.is_empty() {
        Ok((name, GroupOutcome::Merged))
    } else {
        Ok((name, GroupOutcome::Fallback(missing)))
    }
}

/// Result summary of an apply run.
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub merged: usize,
    pub skipped: usize,
    pub fallback: usize,
    pub swapped: usize,
    pub deleted: Vec<String>,
}

/// Result summary of a sync run.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub checked: usize,
    pub ok: usize,
    pub repaired: usize,
    pub repaired_bytes: u64,
    /// Files present on disk that were not in the manifest and were removed.
    /// Closes the "exists on disk but isn't in the manifest" case that pure
    /// verify+repair can't catch on its own.
    pub orphans_removed: usize,
    pub failed: Vec<String>,
}

/// Result of a server checkout.
#[derive(Debug, Clone)]
pub struct CheckoutReport {
    pub from_server: Server,
    pub to_server: Server,
    pub swapped_files: usize,
    pub new_version: String,
}

/// Result of a from-zero install.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub version: String,
    pub sync: SyncReport,
    /// Relative path of the game executable inside the install (for launching
    /// via Steam/Proton). None if no `.exe` was found.
    pub game_exe: Option<String>,
}

/// Locate the game executable inside an installed game folder.
pub fn find_game_exe(folder: &Path) -> Option<String> {
    let candidates = [
        "Client/Binaries/Win64/Client-Win64-Shipping.exe",
        "Client/Binaries/Win64/PGR.exe",
        "PGR.exe",
    ];
    for c in candidates {
        if folder.join(c).is_file() {
            return Some(c.to_string());
        }
    }
    // fallback: any .exe directly in the root
    if let Ok(rd) = std::fs::read_dir(folder) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".exe") && e.path().is_file() {
                return Some(name);
            }
        }
    }
    None
}

fn is_krpdiff(dest: &str) -> bool {
    dest.to_ascii_lowercase().ends_with(".krpdiff")
}

fn file_matches(path: &Path, size: u64, md5: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.len() == size && !md5.is_empty() => {
            kuro_patch::md5_file(path).map(|a| a == md5).unwrap_or(false)
        }
        Ok(m) if m.len() == size => true,
        _ => false,
    }
}

/// True when every dst file already exists at the expected size (group was
/// already applied). Size-only check for now; TODO: spot-check first file MD5
/// like ww-manager does.
fn group_already_target(game_folder: &Path, group: &GroupInfo) -> bool {
    !group.dst_files.is_empty()
        && group.dst_files.iter().all(|d| {
            let p = game_folder.join(d.dest.trim_start_matches('/'));
            std::fs::metadata(p).map(|m| m.len() == d.size).unwrap_or(false)
        })
}

/// Walk `game_folder` and remove any file the manifest does not list. Returns
/// the number of files removed. Never deletes directories that still contain
/// other files; prunes directories that became empty as a side effect.
///
/// Excluded from deletion:
/// - `launcherDownloadConfig.json` (the live local config we wrote)
/// - `*.tmp` and `*.bak` (in-flight repair / backup of the file they sit next to)
/// - anything inside `.incremental_download/` (predownload staging)
///
/// Path comparison is on forward-slash relative paths, matching the manifest's
/// `dest` convention regardless of host OS.
fn sweep_orphans(game_folder: &Path, manifest: &[ResourceItem]) -> Result<usize> {
    use std::collections::HashSet;

    let manifest_set: HashSet<String> = manifest
        .iter()
        .map(|r| r.dest.trim_start_matches('/').replace('\\', "/"))
        .collect();

    // collect orphans first, delete after — mutating the tree while walking it
    // is a recipe for skipped entries on some platforms
    let mut orphans: Vec<PathBuf> = Vec::new();
    for entry in walk_files(game_folder)? {
        let rel = entry
            .strip_prefix(game_folder)
            .map_err(|e| Error::Patch(format!("orphan walk prefix: {e}")))?
            .to_string_lossy()
            .replace('\\', "/");

        if is_protected(&rel) {
            continue;
        }
        if manifest_set.contains(&rel) {
            continue;
        }
        orphans.push(entry);
    }

    for path in &orphans {
        let _ = std::fs::remove_file(path);
    }

    // prune empty directories bottom-up, but only inside the game folder and
    // never the folder itself. Walking the full tree is cheap relative to a
    // full install and keeps the logic correct for arbitrarily deep removals.
    prune_empty_dirs(game_folder);

    Ok(orphans.len())
}

/// Recursively prune any empty directory under `root`, never removing `root`
/// itself. Best-effort: IO errors are ignored because the worst case is
/// leaving an empty directory behind, not corrupting data.
fn prune_empty_dirs(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path == root {
            continue;
        }
        prune_empty_dirs(&path);
        if is_dir_empty(&path) {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

fn is_dir_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
}

/// Recursive file walker that does not follow the protected `.incremental_download`
/// directory (we never want to touch predownload staging from sync).
fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn recurse(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if name == ".incremental_download" {
                    continue;
                }
                recurse(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    recurse(root, &mut out).map_err(|e| Error::Patch(format!("orphan walk: {e}")))?;
    Ok(out)
}

/// Files / directories the sweep must never delete.
fn is_protected(rel: &str) -> bool {
    if rel == "launcherDownloadConfig.json" {
        return true;
    }
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".tmp") || lower.ends_with(".bak") {
        return true;
    }
    false
}
