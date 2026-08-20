//! On-disk state: local version config + staging/cache directory layout.

use std::path::{Path, PathBuf};

use kuro_api::{LocalConfig, Result};

/// `launcherDownloadConfig.json` — version + appId written by the official
/// launcher and by us after an update.
pub const LOCAL_CONFIG_FILE: &str = "launcherDownloadConfig.json";
/// Staging dir for downloaded patches / full files (mirrors ww-manager's
/// `.incremental_download`).
pub const INCREMENTAL_DIR: &str = ".incremental_download";
/// Tool cache (md5 cache, saved indexes).
pub const CACHE_DIR: &str = ".kuro_cache";

pub fn incremental_dir(game_folder: &Path) -> PathBuf {
    game_folder.join(INCREMENTAL_DIR)
}

pub fn cache_dir(game_folder: &Path) -> PathBuf {
    game_folder.join(CACHE_DIR)
}

/// Staged path of a downloaded krpdiff file.
pub fn staged_patch_path(game_folder: &Path, name: &str) -> PathBuf {
    incremental_dir(game_folder).join(name)
}

/// Staged path of a full-file fallback download.
pub fn staged_resource_path(game_folder: &Path, dest: &str) -> PathBuf {
    incremental_dir(game_folder).join("resources").join(dest.trim_start_matches('/'))
}

pub fn read_local_config(game_folder: &Path) -> Result<Option<LocalConfig>> {
    let path = game_folder.join(LOCAL_CONFIG_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&data)?))
}

pub fn write_local_config(game_folder: &Path, cfg: &LocalConfig) -> Result<()> {
    let path = game_folder.join(LOCAL_CONFIG_FILE);
    let data = serde_json::to_string_pretty(cfg)?;
    std::fs::write(path, data)?;
    Ok(())
}
