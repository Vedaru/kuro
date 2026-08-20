//! `GameManager` — the orchestrator: status / predownload / apply / sync.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kuro_api::config::ServerEntry;
use kuro_api::{
    game_server_by_app_id, server_entry, ApiClient, Error, FileRef, Game, GroupInfo, LocalConfig,
    PatchConfig, PatchIndex, ResourceItem, Server, Result,
};

use crate::atomic::{recover_backup, safe_replace};
use crate::download::{download_chunked, download_single};
use crate::state::{self, incremental_dir};

/// Events emitted during long operations (for the TUI / progress UI).
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Log(String),
    GroupStart { name: String },
    GroupDone { name: String, bytes: u64 },
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

const CHUNK_CONCURRENCY: usize = 8;
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
        let index = self.api.fetch_index(self.server_entry().api_url).await?;
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

        let index = self.api.fetch_index(self.server_entry().api_url).await?;
        let cdn = self.api.pick_cdn(&index)?.url.clone();
        let to_version = index.default.version.clone();

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
        let _ = tx.send(ProgressEvent::Log(format!(
            "predownload {} -> {} ({} groups, {:.1} GiB)",
            plan.from_version,
            plan.to_version,
            plan.patch_groups.len(),
            plan.total_bytes as f64 / (1 << 30) as f64
        )));
        let index = self.api.fetch_index(self.server_entry().api_url).await?;
        let cdn = self.api.pick_cdn(&index)?.url.clone();
        let patch_cfg = index
            .default
            .config
            .patch_config
            .iter()
            .find(|p| p.version == plan.from_version)
            .ok_or_else(|| Error::MissingField("patchConfig entry for local version"))?;

        let dir = incremental_dir(&self.game_folder);
        std::fs::create_dir_all(&dir)?;
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
            let _ = tx.send(ProgressEvent::GroupStart { name: group.name.clone() });
            let url = ApiClient::krpdiff_url(&cdn, patch_cfg, &group.name);
            let staged = state::staged_patch_path(&self.game_folder, &group.name);
            let tmp = staged.with_extension("krpdiff.tmp");
            download_single(&self.http, &url, &tmp, Some(group.size), None).await?;
            std::fs::rename(&tmp, &staged)?;
            let _ = tx.send(ProgressEvent::GroupDone {
                name: group.name.clone(),
                bytes: group.size,
            });
        }

        for item in &plan.full_files {
            if item.local_ready {
                continue;
            }
            let _ = tx.send(ProgressEvent::GroupStart { name: item.name.clone() });
            let res = res_by_dest
                .get(item.name.as_str())
                .ok_or_else(|| Error::MissingField("resource entry"))?;
            let from = res.from_folder.as_deref().ok_or(Error::MissingField("fromFolder"))?;
            let url = ApiClient::resource_url(&cdn, from, &res.dest);
            let staged = state::staged_resource_path(&self.game_folder, &res.dest);
            std::fs::create_dir_all(staged.parent().unwrap())?;
            let tmp = staged.with_extension("tmp");
            if res.chunk_infos.is_empty() {
                download_single(&self.http, &url, &tmp, Some(res.size), Some(&res.md5)).await?;
            } else {
                download_chunked(
                    &self.http,
                    &url,
                    &tmp,
                    &res.chunk_infos,
                    Some(&res.md5),
                    CHUNK_CONCURRENCY,
                )
                .await?;
            }
            std::fs::rename(&tmp, &staged)?;
            let _ = tx.send(ProgressEvent::GroupDone {
                name: item.name.clone(),
                bytes: item.size,
            });
        }

        let _ = tx.send(ProgressEvent::Done);
        Ok(())
    }

    /// Apply a downloaded incremental update: merge krpdiffs natively, verify,
    /// then atomically swap into the game folder. The game must not be running.
    pub async fn apply(&self) -> Result<ApplyReport> {
        let from_version = self
            .local_version()?
            .ok_or_else(|| Error::NoLocalConfig(self.game_folder.clone()))?;

        let index = self.api.fetch_index(self.server_entry().api_url).await?;
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
                download_single(&self.http, &url, &tmp, Some(dst.size), Some(&dst.md5)).await?;
            } else {
                download_chunked(&self.http, &url, &tmp, &chunks, Some(&md5), CHUNK_CONCURRENCY)
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

    /// Full-tree MD5 verify/repair against the remote index.
    pub async fn sync(&self) -> Result<()> {
        Err(Error::Unimplemented("sync — next milestone"))
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
