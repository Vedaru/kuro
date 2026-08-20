//! `GameManager` — the orchestrator: status / predownload / apply / sync.

use std::path::{Path, PathBuf};

use kuro_api::config::ServerEntry;
use kuro_api::{
    game_server_by_app_id, server_entry, ApiClient, Error, Game, GroupInfo, ResourceItem, Server,
    Result,
};

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
    /// then atomically swap into the game folder.
    ///
    /// TODO(next milestone): port ww-manager's staging flow —
    ///   1. for each group: `kuro_patch::apply_krdiff(game, krpdiff, out)` into a temp dir
    ///   2. MD5-verify every output against `dstFiles`
    ///   3. move outputs to `.incremental_download/staged/`
    ///   4. `atomic::safe_replace` each into the game dir (with `.bak` recovery)
    ///   5. handle `deleteFiles`, fall back to full-file download when a merge fails
    ///   6. update `launcherDownloadConfig.json` to the target version
    pub async fn apply(&self) -> Result<()> {
        Err(Error::Unimplemented("apply — next milestone (see doc comment)"))
    }

    /// Full-tree MD5 verify/repair against the remote index.
    pub async fn sync(&self) -> Result<()> {
        Err(Error::Unimplemented("sync — next milestone"))
    }
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
