//! `GameManager` — the orchestrator: status / predownload / apply / sync.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kuro_api::config::ServerEntry;
use kuro_api::{
    build_client, game_server_by_app_id, index_url, server_entry, ApiClient, ChunkInfo, Error,
    FileRef, Game, GroupInfo, LauncherIndex, LocalConfig, PatchConfig, PatchIndex, ResourceItem,
    Server, Result,
};

use crate::atomic::{recover_backup, safe_replace};
use crate::download::{download_chunked, download_single, Budget};
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

/// Global ceiling on in-flight CDN requests for the whole run — every file and
/// every byte range draws from this one pool (see [`Budget`]).
///
/// The Kuro CDN throttles per *connection*: a single edge streamed
/// ~0.05–0.10 MB/s however fast the client's link was, so throughput comes
/// from concurrency. But only up to a point — measured 2026-09, aggregate
/// throughput scaled ~linearly to 8 connections (~0.6 MB/s against a ~0.8 MB/s
/// machine ceiling), and at 12+ the fan-out itself provoked stalls (a 32-way
/// batch hung a 4 MiB ranged GET for 52.3s while its peers finished in
/// 2.8–8.4s). 8 is the measured sweet spot.
///
/// Sharing one budget across the run is the point: a big pak's ranges and a
/// small file's request compete for the same 8 slots instead of each file
/// reserving its own, so the tail of one file no longer leaves the link idle
/// while the next file waits its turn.
const DOWNLOAD_CONCURRENCY: usize = 8;
/// Range tasks a single chunked file may have outstanding at once. Actual
/// connections are capped by `DOWNLOAD_CONCURRENCY`; this only sizes one
/// file's fan-out.
const CHUNK_CONCURRENCY: usize = 8;
/// CN 3.6.0 manifests carry no chunkInfos — synthesize fixed-size ranges so
/// large paks download over parallel connections (CDNs rate-limit per conn).
const SYNTH_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// Concurrent files the download loops keep in flight. The `JoinSet` still
/// spawns a task per file, but this semaphore gates how many run at once, so it
/// bounds open file handles and in-flight work: enough files that a small file
/// fills a slot a big file's tail would leave idle, without a run-wide list of
/// open descriptors.
const FILE_CONCURRENCY: usize = 8;
/// Parallel krpdiff merges during apply (CPU-bound, native engine).
const MERGE_CONCURRENCY: usize = 4;

pub struct GameManager {
    pub game_folder: PathBuf,
    pub game: Game,
    pub server: Server,
    api: ApiClient,
    http: reqwest::Client,
    /// Run-wide connection budget shared by every download this manager starts.
    budget: Budget,
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
        let http = build_client()?;
        Ok(Self {
            game_folder,
            game,
            server,
            api,
            http,
            budget: Budget::new(DOWNLOAD_CONCURRENCY),
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
        let nodes = cdn_nodes(&self.api, &index)?;
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
        let patch_index = self.api.fetch_manifest(&nodes, &patch_cfg.index_file).await?;

        Ok(self.plan_from_patch_index(&patch_index, &from_version, &to_version))
    }

    /// Build a plan from an already-fetched patch manifest (no network, so it
    /// is directly testable).
    fn plan_from_patch_index(
        &self,
        patch_index: &PatchIndex,
        from_version: &str,
        to_version: &str,
    ) -> PredownloadPlan {
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
            // No `fromFolder` is normal — the file is served off the patch's own
            // base (see `resource_base`). Demanding it here is how a real WuWa
            // CN update (3.6.0 -> 3.6.1: 173 files, 1.49 GiB, *no* fromFolder
            // on any entry) ended up planning zero bytes instead.
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

        PredownloadPlan {
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            patch_groups,
            full_files,
            total_bytes: total,
        }
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
        let nodes = cdn_nodes(&self.api, &index)?;

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
        let patch_index = self.api.fetch_manifest(&nodes, &patch_cfg.index_file).await?;
        let res_by_dest: std::collections::HashMap<&str, &ResourceItem> = patch_index
            .resource
            .iter()
            .map(|r| (r.dest.as_str(), r))
            .collect();

        // Both passes run across FILES in parallel, each file bounded by the
        // shared connection budget. Sequentially, one file's slow tail left the
        // link idle while the rest queued behind it; the TUI keys events by
        // file name, so completion order does not matter.
        let sem = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
        let mut handles = tokio::task::JoinSet::new();

        for group in &plan.patch_groups {
            if group.local_ready {
                continue;
            }
            let urls = node_urls(&nodes, |base| ApiClient::krpdiff_url(base, patch_cfg, &group.name));
            let staged = state::staged_patch_path(&self.game_folder, &group.name);
            let tmp = tmp_sibling(&staged, "krpdiff");
            let name = group.name.clone();
            let size = group.size;
            let http = self.http.clone();
            let budget = self.budget.clone();
            let tx = tx.clone();
            let sem = sem.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let _ = tx.send(ProgressEvent::GroupStart { name: name.clone() }).await;
                let _ = tx
                    .send(ProgressEvent::FileProgress {
                        name: name.clone(),
                        bytes: 0,
                        total: size,
                    })
                    .await;
                download_single(&http, &urls, &tmp, Some(size), None, &name, Some(&tx), &budget)
                    .await?;
                std::fs::rename(&tmp, &staged)?;
                let _ = tx.send(ProgressEvent::GroupDone { name, bytes: size }).await;
                Ok::<_, Error>(())
            });
        }

        for item in &plan.full_files {
            if item.local_ready {
                continue;
            }
            let res = res_by_dest
                .get(item.name.as_str())
                .ok_or_else(|| Error::MissingField("resource entry"))?;
            let from = resource_base(res, patch_cfg);
            let urls = node_urls(&nodes, |base| ApiClient::resource_url(base, &from, &res.dest));
            let staged = state::staged_resource_path(&self.game_folder, &res.dest);
            std::fs::create_dir_all(staged.parent().unwrap())?;
            let tmp = tmp_sibling(&staged, "dl");
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
            let size = res.size;
            let md5 = res.md5.clone();
            let dest = res.dest.clone();
            let name = item.name.clone();
            let item_size = item.size;
            let http = self.http.clone();
            let budget = self.budget.clone();
            let tx = tx.clone();
            let sem = sem.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let _ = tx.send(ProgressEvent::GroupStart { name: name.clone() }).await;
                let _ = tx
                    .send(ProgressEvent::FileProgress {
                        name: name.clone(),
                        bytes: 0,
                        total: item_size,
                    })
                    .await;
                if chunk_infos.is_empty() {
                    download_single(
                        &http,
                        &urls,
                        &tmp,
                        Some(size),
                        Some(md5.as_str()),
                        &dest,
                        Some(&tx),
                        &budget,
                    )
                    .await?;
                } else {
                    download_chunked(
                        &http,
                        &urls,
                        &tmp,
                        &chunk_infos,
                        Some(md5.as_str()),
                        CHUNK_CONCURRENCY,
                        &dest,
                        Some(&tx),
                        &budget,
                    )
                    .await?;
                }
                std::fs::rename(&tmp, &staged)?;
                let _ = tx
                    .send(ProgressEvent::GroupDone {
                        name,
                        bytes: item_size,
                    })
                    .await;
                Ok::<_, Error>(())
            });
        }

        while let Some(joined) = handles.join_next().await {
            joined
                .map_err(|e| Error::Patch(format!("predownload join: {e}")))??;
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
        let nodes = cdn_nodes(&self.api, &index)?;
        let patch_cfg = index
            .default
            .config
            .patch_config
            .iter()
            .find(|p| p.version == from_version)
            .ok_or_else(|| Error::MissingField("patchConfig entry for local version"))?;
        let patch_index = self.api.fetch_manifest(&nodes, &patch_cfg.index_file).await?;

        self.apply_inner(&patch_index, &nodes, patch_cfg, &remote).await
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
        cdn_nodes: &[&str],
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
        let sem = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
        let mut handles = tokio::task::JoinSet::new();
        for dst in fallback_dests {
            if !seen.insert(dst.dest.clone()) {
                continue;
            }
            let res = res_by_dest.get(&dst.dest);
            let (from, chunks, md5) = match res {
                Some(r) if r.from_folder.is_some() => (
                    r.from_folder.clone().unwrap(),
                    r.chunk_infos.clone(),
                    r.md5.clone(),
                ),
                _ => {
                    // fall back to the patch's zip base
                    (patch_cfg.base_url.clone(), vec![], String::new())
                }
            };
            let urls = node_urls(cdn_nodes, |base| ApiClient::resource_url(base, &from, &dst.dest));
            let staged = state::staged_patch_path(&self.game_folder, &dst.dest);
            std::fs::create_dir_all(staged.parent().unwrap())?;
            let tmp = tmp_sibling(&staged, "dl");
            let size = dst.size;
            let dst_md5 = dst.md5.clone();
            let name = dst.dest.clone();
            let http = self.http.clone();
            let budget = self.budget.clone();
            let sem = sem.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                if chunks.is_empty() {
                    download_single(
                        &http,
                        &urls,
                        &tmp,
                        Some(size),
                        Some(dst_md5.as_str()),
                        &name,
                        None,
                        &budget,
                    )
                    .await?;
                } else {
                    download_chunked(
                        &http,
                        &urls,
                        &tmp,
                        &chunks,
                        Some(md5.as_str()),
                        CHUNK_CONCURRENCY,
                        &name,
                        None,
                        &budget,
                    )
                    .await?;
                }
                std::fs::rename(&tmp, &staged)?;
                Ok::<_, Error>(())
            });
        }
        while let Some(joined) = handles.join_next().await {
            joined
                .map_err(|e| Error::Patch(format!("fallback download join: {e}")))??;
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
            let game_path = self.game_folder.join(normalise_dest(dest));
            // `safe_replace` creates the parent directory, so a group that adds
            // a directory the install never had (e.g. Client/Content/HD) is fine.
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
            let game_path = self.game_folder.join(normalise_dest(&item.dest));
            std::fs::create_dir_all(game_path.parent().unwrap())?;
            recover_backup(&game_path)?;
            safe_replace(&staged, &game_path)?;
            report.swapped += 1;
        }

        // ---- delete phase ----
        for f in &patch_index.delete_files {
            let game_path = self.game_folder.join(normalise_dest(f));
            if game_path.exists() {
                std::fs::remove_file(&game_path)?;
                report.deleted.push(f.clone());
            }
        }

        // ---- guard: never record a version we did not actually install ----
        // Writing `to_version` regardless is how a no-op apply (a patch
        // manifest whose entries were all skipped) left the launcher config
        // claiming 3.6.1 while the client was still on 3.6.0 — after which
        // `status` reports "up to date" forever and the real update is missed.
        let mut not_installed: Vec<String> = Vec::new();
        for item in &patch_index.resource {
            if is_krpdiff(&item.dest) {
                continue;
            }
            let game_path = self.game_folder.join(normalise_dest(&item.dest));
            if !file_matches(&game_path, item.size, &item.md5) {
                not_installed.push(item.dest.clone());
            }
        }
        for group in &patch_index.group_infos {
            for dst in &group.dst_files {
                if is_krpdiff(&dst.dest) || complete_dests.contains(&dst.dest) {
                    continue;
                }
                let game_path = self.game_folder.join(normalise_dest(&dst.dest));
                if !file_matches(&game_path, dst.size, &dst.md5) {
                    not_installed.push(dst.dest.clone());
                }
            }
        }
        if !not_installed.is_empty() {
            let sample = not_installed
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::Patch(format!(
                "refusing to record {to_version} as installed: {} target file(s) are not at \
                 the target revision (e.g. {sample}) — the update is not applied yet; \
                 rerun predownload (then apply) or sync, staging kept",
                not_installed.len()
            )));
        }

        // ---- finish: bump version, clean staging ----
        let cfg = LocalConfig {
            version: to_version.to_string(),
            app_id: self.server_entry().app_id.to_string(),
            group: "default".to_string(),
        };
        state::write_local_config(&self.game_folder, &cfg)?;
        // Best-effort: the version is already recorded, so failing the whole
        // apply because staging was busy/locked would report failure for a
        // successful update. The next predownload reuses or clears the dir.
        let _ = std::fs::remove_dir_all(&inc);

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
        // `open()` (and the sync that follows) resolve game+server from this
        // file, so it has to be written before we can sync — but a failed sync
        // must not leave it claiming a version we never installed. Keep the
        // previous bytes and put them back on failure.
        let cfg_path = game_folder.join(state::LOCAL_CONFIG_FILE);
        let prev_cfg = std::fs::read(&cfg_path).ok();
        let cfg = LocalConfig {
            version: version.clone(),
            app_id: entry.app_id.to_string(),
            group: "default".to_string(),
        };
        state::write_local_config(&game_folder, &cfg)?;

        let mgr = Self::open(game_folder.clone()).await?;
        let sync = match mgr.sync_with_progress(tx).await {
            Ok(report) => report,
            Err(e) => {
                match prev_cfg {
                    Some(bytes) => {
                        let _ = std::fs::write(&cfg_path, bytes);
                    }
                    None => {
                        let _ = std::fs::remove_file(&cfg_path);
                    }
                }
                return Err(e);
            }
        };
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
        let nodes = cdn_nodes(&self.api, &index)?;
        let cfg = &index.default.config;
        let to_version = cfg.version.clone();
        let base = cfg.base_url.clone();

        // full index → md5s for the diff files
        let full_index = self.api.fetch_manifest(&nodes, &cfg.index_file).await?;
        let md5_by_dest: HashMap<String, String> = full_index
            .resource
            .iter()
            .map(|r| (r.dest.clone(), r.md5.clone()))
            .collect();

        let mut swapped = 0;
        let sem = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
        let mut handles = tokio::task::JoinSet::new();
        for f in entry.diff_files {
            let Some(expected_md5) = md5_by_dest.get(*f) else {
                continue;
            };
            let urls = node_urls(&nodes, |n| ApiClient::resource_url(n, &base, f));
            let game_path = self.game_folder.join(f.trim_start_matches('/'));
            std::fs::create_dir_all(game_path.parent().unwrap())?;
            let tmp = game_path.with_extension("checkout.tmp");
            let name = f.to_string();
            let md5 = expected_md5.clone();
            let http = self.http.clone();
            let budget = self.budget.clone();
            let sem = sem.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                download_single(&http, &urls, &tmp, None, Some(md5.as_str()), &name, None, &budget)
                    .await?;
                safe_replace(&tmp, &game_path)?;
                Ok::<_, Error>(())
            });
        }
        while let Some(joined) = handles.join_next().await {
            joined.map_err(|e| Error::Patch(format!("checkout join: {e}")))??;
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
        let nodes = cdn_nodes(&self.api, &index)?;
        let cfg = &index.default.config;
        let full_index = self.api.fetch_manifest(&nodes, &cfg.index_file).await?;
        self.sync_inner_with_progress(&full_index, &nodes, &cfg.base_url, tx)
            .await
    }

    /// Sync core, testable with a synthetic index + local HTTP server.
    pub async fn sync_inner(
        &self,
        full_index: &PatchIndex,
        cdn_nodes: &[&str],
        base: &str,
    ) -> Result<SyncReport> {
        self.sync_inner_with_progress(full_index, cdn_nodes, base, None).await
    }

    /// Sync core with progress events.
    pub async fn sync_inner_with_progress(
        &self,
        full_index: &PatchIndex,
        cdn_nodes: &[&str],
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
        let cache = crate::md5_cache::Md5Cache::load(&self.game_folder);
        let verify_cache = cache.clone();
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
                    let key = normalise_dest(&item.dest);
                    let p = game_folder.join(&key);
                    let meta = std::fs::metadata(&p).ok();
                    let ok = match &meta {
                        Some(m) if m.len() == item.size => {
                            item.md5.is_empty() || verify_cache.matches(&key, &p, m, &item.md5)
                        }
                        _ => false,
                    };
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
        let sem = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
        let mut handles = tokio::task::JoinSet::new();
        let cdn_nodes: Arc<Vec<String>> = Arc::new(cdn_nodes.iter().map(|s| s.to_string()).collect());
        for item in to_fix {
            let sem = sem.clone();
            let http = self.http.clone();
            let budget = self.budget.clone();
            let game_folder = self.game_folder.clone();
            let cache = cache.clone();
            let cdn_nodes = cdn_nodes.clone();
            let base = base.to_string();
            let tx = tx.clone();
            handles.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let from = item.from_folder.clone().unwrap_or_else(|| base.clone());
                let urls = node_urls(&cdn_nodes, |n| ApiClient::resource_url(n, &from, &item.dest));
                let key = normalise_dest(&item.dest);
                let game_path = game_folder.join(&key);
                std::fs::create_dir_all(game_path.parent().unwrap())?;
                let tmp = tmp_sibling(&game_path, "sync");
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
                        &chunk_infos,
                        Some(item.md5.as_str()),
                        CHUNK_CONCURRENCY,
                        &item.dest,
                        tx.as_ref(),
                        &budget,
                    )
                    .await?;
                }
                safe_replace(&tmp, &game_path)?;
                cache.record_file(&key, &game_path, &item.md5);
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

        // Persist the hash cache — best effort: a failed write only costs the
        // re-hash it was meant to save and must not fail a good sync.
        let _ = cache.save();

        // orphan sweep — stale build artefacts only (see `sweep_orphans`: the
        // manifest covers the base client alone, so "not listed" must never
        // mean "delete" for state the game creates for itself).
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
    /// Stale build artefacts removed — files not in the manifest that sat in a
    /// manifest directory with a manifest extension. Live game state (the
    /// client's resource channel, saves, settings, logs, SDK payloads) is never
    /// counted here because it is never removed; see [`sweep_orphans`].
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

/// Manifest paths are CDN paths: forward slashes, no drive prefix, relative to
/// the install root (a stray leading `/` also shows up in the live files).
/// Normalise once so every join and comparison agrees.
fn normalise_dest(dest: &str) -> String {
    dest.trim_start_matches('/').replace('\\', "/")
}

fn split_dest(dest: &str) -> (&str, &str) {
    match dest.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", dest),
    }
}

fn extension_of(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
}

/// Temp path for an in-flight download, sitting next to its destination.
///
/// Appends instead of replacing the extension: `Path::with_extension` maps
/// `pakchunk0.pak` and `pakchunk0.sig` — siblings the CDN always ships
/// together, and repairs/downloads run several files at once — onto the same
/// `pakchunk0.tmp`, so two concurrent writers can clobber each other's
/// half-written file. The `.tmp` suffix also keeps it protected from the
/// orphan sweep.
fn tmp_sibling(path: &Path, tag: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{tag}.tmp"));
    PathBuf::from(s)
}

/// CDN folder a full-file entry is fetched from. Entries without `fromFolder`
/// (every entry of the live WuWa CN hotfix manifest) are served straight off
/// the patch's own base URL — the same fallback `sync` uses for the full
/// manifest.
fn resource_base(res: &ResourceItem, patch_cfg: &PatchConfig) -> String {
    res.from_folder
        .clone()
        .unwrap_or_else(|| patch_cfg.base_url.clone())
}

/// CDN base URLs to try for this run, best edge first (borrows from `index`).
fn cdn_nodes<'a>(api: &ApiClient, index: &'a LauncherIndex) -> Result<Vec<&'a str>> {
    Ok(api
        .cdn_candidates(index)?
        .iter()
        .map(|n| n.url.as_str())
        .collect())
}

/// Build one resource's URL on every candidate CDN edge, best first.
///
/// The download primitives take this list and fail over to the next edge when
/// one stalls, so a request is never pinned to the single node that was picked
/// when the operation started.
fn node_urls<S: AsRef<str>>(nodes: &[S], build: impl Fn(&str) -> String) -> Vec<String> {
    nodes.iter().map(|n| build(n.as_ref())).collect()
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

/// Walk `game_folder` and remove stale build artefacts — files the manifest
/// does not list *and* that sit in a directory the manifest ships into, with
/// an extension the manifest uses there. Returns the number removed. Never
/// deletes directories that still contain other files; prunes directories that
/// became empty as a side effect.
///
/// The narrowing is the whole point. Kuro's CDN manifest describes **only** the
/// base client: it has zero entries for `Client/Saved/**`, and none for the
/// SDK / anti-cheat / crash-reporter payloads the client writes for itself. A
/// blanket "delete whatever isn't listed" sweep therefore wipes live game state
/// — the client's resource + video channel (tens of GB), graphics settings,
/// local storage, saves, crash state — and the client's very next launch starts
/// re-downloading all of it from zero. That is a self-inflicted 30 GB repair.
///
/// Always excluded from deletion:
/// - `launcherDownloadConfig.json` (the live local config we wrote)
/// - `Client/Saved/**` and the client's runtime segments/artefacts — see
///   [`is_protected`]
/// - `*.tmp` / `*.bak` (in-flight repair / backup of the file they sit next to)
/// - anything inside `.incremental_download/` (predownload staging)
///
/// Path comparison is on forward-slash relative paths, matching the manifest's
/// `dest` convention regardless of host OS.
fn sweep_orphans(game_folder: &Path, manifest: &[ResourceItem]) -> Result<usize> {
    use std::collections::HashSet;

    let manifest_set: HashSet<String> = manifest.iter().map(|r| normalise_dest(&r.dest)).collect();
    let dir_exts = manifest_dir_extensions(manifest);

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
        if !is_stale_artifact(&rel, &dir_exts) {
            continue; // live game state we have no way to restore
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

/// For every directory the manifest ships files into, the set of lowercase
/// extensions it uses there.
fn manifest_dir_extensions(manifest: &[ResourceItem]) -> HashMap<String, HashSet<String>> {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    for r in manifest {
        let dest = normalise_dest(&r.dest);
        let (dir, name) = split_dest(&dest);
        if let Some(ext) = extension_of(name) {
            map.entry(dir.to_string()).or_default().insert(ext);
        }
    }
    map
}

/// True when `rel` looks like a stale build artefact rather than live game
/// state: its directory is one the manifest ships into, and its extension is
/// one the manifest uses there. Anything else (unknown directory, foreign
/// extension) is left alone — the cost of keeping a stray file is zero, the
/// cost of deleting live state is a full re-download or lost saves.
fn is_stale_artifact(rel: &str, dir_exts: &HashMap<String, HashSet<String>>) -> bool {
    let (dir, name) = split_dest(rel);
    let Some(exts) = dir_exts.get(dir) else {
        return false;
    };
    match extension_of(name) {
        Some(ext) => exts.contains(&ext),
        None => false,
    }
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

/// Files the sweep must never delete, whatever the manifest says.
///
/// The manifest only covers the base client, so "not in the manifest" is not a
/// synonym for "stale". Everything matched here is state the *game* owns, is
/// not on the CDN, and cannot be repaired — only re-downloaded by the client
/// on its next launch (for the resource channel: tens of gigabytes).
fn is_protected(rel: &str) -> bool {
    if rel == "launcherDownloadConfig.json" {
        return true;
    }
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".tmp") || lower.ends_with(".bak") {
        return true;
    }
    // The client's own tree: resource/video channel (Resources/**, Video/Paks),
    // config, saves, local storage, logs, crash reports. The live WuWa CN
    // manifest has zero entries under here.
    if lower.starts_with("client/saved/") {
        return true;
    }
    // Runtime segments the client, its SDK and its crash reporter create at any
    // depth (including outside Client/Saved).
    const RUNTIME_SEGMENTS: &[&str] = &[
        "crashsightlog/",
        "crashsight64/",
        "wesight/",
        "pipe_client/",
        "crashreportclient/",
        ".quality/",
    ];
    if RUNTIME_SEGMENTS.iter().any(|seg| lower.contains(seg)) {
        return true;
    }
    // Runtime artefacts by extension: logs, crash dumps, local databases,
    // per-machine hashes, local saves.
    const RUNTIME_SUFFIXES: &[&str] = &[
        ".log",
        ".dmp",
        ".dmp.gz",
        ".db",
        ".db-journal",
        ".hash",
        ".sav",
    ];
    if RUNTIME_SUFFIXES.iter().any(|suf| lower.ends_with(suf)) {
        return true;
    }
    // Anti-cheat loader data; regenerated by ACE but not by us.
    if lower.ends_with("/pld.dat") || lower == "pld.dat" {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(dest: &str, size: u64, from_folder: Option<&str>) -> ResourceItem {
        ResourceItem {
            dest: dest.to_string(),
            md5: String::new(),
            size,
            from_folder: from_folder.map(str::to_string),
            chunk_infos: vec![],
        }
    }

    /// Everything the client writes for itself must survive a sweep — these are
    /// the real paths that were deleted on a live WuWa install, taking the
    /// client's resource channel, settings, saves and SDK state with them.
    #[test]
    fn sweep_protects_client_state() {
        for rel in [
            "Client/Saved/Resources/Video/Paks/188_0/Video_188_0-WindowsNoEditor.pak",
            "Client/Saved/Resources/3.6.0/Lang_en/Base/pakchunk10-WindowsNoEditor.pak",
            "Client/Saved/Resources/3.6.0/ResManifest/ManifestAggregated_3.6.17.txt",
            "Client/Saved/Config/WindowsNoEditor/GameUserSettings.ini",
            "Client/Saved/Config/CrashReportClient/UE4CC-1/CrashReportClient.ini",
            "Client/Saved/LocalStorage/LocalStorage.db",
            "Client/Saved/LocalStorage/LocalStorage.db-journal",
            "Client/Saved/Logs/Client.log",
            "Client/Binaries/Win64/AntiCheatExpert/pld.dat",
            "Client/Binaries/Win64/KDData-data.db",
            "Client/Binaries/Win64/CrashSightLog/CrashSight.1789914449.336.log",
            "Client/Binaries/Win64/CrashSight64/dump/GbDump.GbS.1.dmp.gz",
            "Client/Binaries/Win64/wesight/crashsight_data/crash_data.info_1",
            "Client/Binaries/Win64/pipe_client/pipeclient_1.log",
            "Client/Binaries/Win64/.quality/performance/performance_data",
            "launcherDownloadConfig.json",
            "Client/Content/Paks/pakchunk0.pak.sync.tmp",
            "Client/Content/Paks/pakchunk0.pak.bak",
        ] {
            assert!(is_protected(rel), "{rel} must be protected from the sweep");
        }
    }

    /// The sweep's actual job: stale build output the manifest no longer ships.
    #[test]
    fn sweep_only_removes_stale_build_output() {
        let manifest = vec![
            res("Client/Content/Paks/pakchunk0.pak", 10, None),
            res("Client/Content/Paks/pakchunk0.sig", 10, None),
            res("Client/Binaries/Win64/Client-Win64-Shipping.exe", 10, None),
        ];
        let dirs = manifest_dir_extensions(&manifest);

        // dropped build artifacts -> swept (both halves of a pak/sig pair)
        assert!(is_stale_artifact(
            "Client/Content/Paks/pakchunk18-WindowsNoEditor.pak",
            &dirs
        ));
        assert!(is_stale_artifact(
            "Client/Content/Paks/pakchunk18-WindowsNoEditor.sig",
            &dirs
        ));

        // foreign extension in a manifest directory -> kept
        assert!(!is_stale_artifact(
            "Client/Binaries/Win64/KDData-data.db",
            &dirs
        ));
        assert!(!is_stale_artifact(
            "Client/Binaries/Win64/cdbe868d_unique_id_kurodata",
            &dirs
        ));

        // directory the manifest does not ship into -> kept
        assert!(!is_stale_artifact(
            "Client/Saved/Resources/Video/Paks/272_1/Video_272_1-WindowsNoEditor.pak",
            &dirs
        ));
        assert!(!is_stale_artifact(
            "Client/Content/Paks/OldLocale/en-US/old.bin",
            &dirs
        ));
    }

    #[test]
    fn concurrent_downloads_get_distinct_temp_paths() {
        let pak = Path::new("/g/Client/Content/Paks/pakchunk0.pak");
        let sig = Path::new("/g/Client/Content/Paks/pakchunk0.sig");
        let a = tmp_sibling(pak, "sync");
        let b = tmp_sibling(sig, "sync");
        assert_ne!(a, b, "pak and sig must not share a temp file");
        assert!(a.to_string_lossy().ends_with("pakchunk0.pak.sync.tmp"));
        // the sweep must never consider an in-flight download an orphan
        assert!(is_protected(&a.to_string_lossy()));
    }

    #[test]
    fn resource_base_falls_back_to_the_patch_base() {
        let patch_cfg = PatchConfig {
            version: "3.6.0".to_string(),
            index_file: String::new(),
            base_url: "launcher/game/G152/10003/3.6.1/tok/zip/".to_string(),
        };
        assert_eq!(
            resource_base(&res("Client/a.dll", 1, None), &patch_cfg),
            patch_cfg.base_url
        );
        assert_eq!(
            resource_base(&res("Client/a.dll", 1, Some("zip")), &patch_cfg),
            "zip"
        );
    }

    /// Regression: a patch manifest with no `fromFolder` (the live WuWa CN
    /// hotfix manifest) must still plan its files, not report "up to date".
    #[tokio::test]
    async fn plan_covers_entries_without_from_folder() {
        let dir = std::env::temp_dir().join(format!("kuro-plan-nofrom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        state::write_local_config(
            &dir,
            &LocalConfig {
                version: "3.6.0".to_string(),
                app_id: "10003".to_string(),
                group: "default".to_string(),
            },
        )
        .unwrap();

        let patch_index = PatchIndex {
            resource: vec![
                res("Client/Binaries/Win64/a.dll", 100, None),
                res("Client/Content/Paks/b.pak", 200, Some("zip")),
            ],
            delete_files: vec![],
            group_infos: vec![],
            apply_types: vec![],
        };

        let mgr = GameManager::open(dir.clone()).await.unwrap();
        let plan = mgr.plan_from_patch_index(&patch_index, "3.6.0", "3.6.1");
        assert_eq!(plan.full_files.len(), 2, "both entries must be planned");
        assert_eq!(plan.total_bytes, 300);
        assert!(plan.full_files.iter().all(|f| !f.local_ready));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
