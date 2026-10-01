//! Quality presets — which `Client/Content/<Q>/` pak set the client mounts.
//!
//! The client picks a pak set once, at startup, from the `-krqlv=` launch
//! argument (`SD` / `HD` / `UHD`); there is no in-game switch and with no
//! argument at all the client asserts out with `kuro: Use launcher to start
//! game!`. So "choosing a quality" is two separate jobs:
//!
//! 1. record which set the user wants (this module),
//! 2. start the client with the matching argument ([`crate::launch`]).
//!
//! Sets are *not* interchangeable at runtime: mounting a directory that was
//! never downloaded leaves the client without the assets it asks for. Hence
//! [`QualityInfo`], which reports what is actually on disk, and the check the
//! launcher makes before starting a preset that is not installed.
//!
//! This whole scheme belongs to the UE titles only — see
//! `Game::uses_quality_tiers`. PGR (Unity) keeps its assets in `PGR_Data/` and
//! takes no tier argument, so the launcher and UI skip quality entirely for it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use kuro_api::Result;

use crate::state;

/// The three pak sets the client knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Quality {
    /// `Client/Content/SD/` — smallest, lowest textures.
    Sd,
    /// `Client/Content/HD/` — the set the official launcher defaults to.
    Hd,
    /// `Client/Content/UHD/` — largest, needs a 4K-capable GPU.
    Uhd,
}

impl Quality {
    /// Every preset, cheapest first (the order the UI lists them in).
    pub const ALL: [Quality; 3] = [Quality::Sd, Quality::Hd, Quality::Uhd];

    /// Value for `-krqlv=`. The client compares this literally, so the case
    /// matters — `-krqlv=hd` is not `-krqlv=HD`.
    pub fn as_arg(self) -> &'static str {
        match self {
            Quality::Sd => "SD",
            Quality::Hd => "HD",
            Quality::Uhd => "UHD",
        }
    }

    /// Directory name under `Client/Content/` — same three strings.
    pub fn dir_name(self) -> &'static str {
        self.as_arg()
    }

    /// Human name for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Quality::Sd => "standard",
            Quality::Hd => "high",
            Quality::Uhd => "ultra",
        }
    }

    /// Parse a user-typed preset, tolerating the obvious synonyms.
    pub fn parse(s: &str) -> Option<Quality> {
        match s.trim().to_ascii_uppercase().as_str() {
            "SD" | "S" | "STANDARD" | "LOW" => Some(Quality::Sd),
            "HD" | "H" | "HIGH" | "DEFAULT" => Some(Quality::Hd),
            "UHD" | "U" | "ULTRA" => Some(Quality::Uhd),
            _ => None,
        }
    }
}

impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_arg())
    }
}

/// `Client/Content/` inside an install.
pub fn content_dir(game_folder: &Path) -> PathBuf {
    game_folder.join("Client").join("Content")
}

/// One preset's footprint on disk.
#[derive(Debug, Clone)]
pub struct PackInfo {
    pub quality: Quality,
    pub present: bool,
    pub files: usize,
    pub bytes: u64,
}

/// Everything the UI needs to offer a choice: what is selected, what is
/// actually installed.
#[derive(Debug, Clone, Default)]
pub struct QualityInfo {
    pub selected: Option<Quality>,
    pub packs: Vec<PackInfo>,
}

impl QualityInfo {
    pub fn pack(&self, q: Quality) -> Option<&PackInfo> {
        self.packs.iter().find(|p| p.quality == q)
    }

    /// A preset is usable only if its directory actually holds paks.
    pub fn is_installed(&self, q: Quality) -> bool {
        self.pack(q).map(|p| p.present).unwrap_or(false)
    }

    /// Installed presets, cheapest first.
    pub fn installed(&self) -> Vec<Quality> {
        Quality::ALL
            .into_iter()
            .filter(|q| self.is_installed(*q))
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.packs.iter().map(|p| p.bytes).sum()
    }

    /// The preset to write if the user accepts without touching anything:
    /// their saved choice, else the one on disk, else HD.
    pub fn default_choice(&self) -> Quality {
        self.selected
            .or_else(|| self.installed().first().copied())
            .unwrap_or(Quality::Hd)
    }
}

/// Scan the install and read the saved selection.
pub fn info(game_folder: &Path) -> QualityInfo {
    let mut packs = Vec::with_capacity(Quality::ALL.len());
    for q in Quality::ALL {
        let dir = content_dir(game_folder).join(q.dir_name());
        let (files, bytes) = count_paks(&dir);
        packs.push(PackInfo {
            quality: q,
            present: files > 0,
            files,
            bytes,
        });
    }
    QualityInfo {
        selected: selected(game_folder),
        packs,
    }
}

/// The last preset the user chose, if any.
pub fn selected(game_folder: &Path) -> Option<Quality> {
    let data = std::fs::read_to_string(state::quality_file(game_folder)).ok()?;
    let saved: SavedQuality = serde_json::from_str(&data).ok()?;
    Some(saved.quality)
}

/// Remember the user's choice. Written atomically so an interrupted save
/// cannot leave a half-written file that parses as "no selection".
pub fn set_selected(game_folder: &Path, q: Quality) -> Result<()> {
    let path = state::quality_file(game_folder);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&SavedQuality { quality: q })?)?;
    crate::atomic::safe_replace(&tmp, &path)
}

#[derive(Serialize, Deserialize)]
struct SavedQuality {
    quality: Quality,
}

/// Total pak files and bytes under `dir` (recursive; the sets are flat but
/// this does not care).
fn count_paks(dir: &Path) -> (usize, u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let (mut files, mut bytes) = (0usize, 0u64);
    for entry in rd.flatten() {
        let path = entry.path();
        // Do not follow symlinks: a link back up the tree (or to `/`) would
        // recurse without bound. `file_type()` is the entry's own type.
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let (f, b) = count_paks(&path);
            files += f;
            bytes += b;
        } else if ft.is_file()
            && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pak"))
        {
            files += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (files, bytes)
}

/// Render the `-krqlv` argument. Used by [`crate::launch`] and by the UI when
/// it shows the user what will be passed.
pub fn quality_arg(q: Quality) -> String {
    format!("-krqlv={}", q.as_arg())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_strings_match_the_client() {
        // The client matches these literally against "SD"/"HD"/"UHD".
        assert_eq!(Quality::Sd.as_arg(), "SD");
        assert_eq!(Quality::Hd.as_arg(), "HD");
        assert_eq!(Quality::Uhd.as_arg(), "UHD");
        assert_eq!(quality_arg(Quality::Uhd), "-krqlv=UHD");
    }

    #[test]
    fn parse_accepts_synonyms_and_is_case_insensitive() {
        assert_eq!(Quality::parse("hd"), Some(Quality::Hd));
        assert_eq!(Quality::parse(" UHD "), Some(Quality::Uhd));
        assert_eq!(Quality::parse("low"), Some(Quality::Sd));
        assert_eq!(Quality::parse("medium"), None);
    }

    #[test]
    fn missing_install_reports_nothing_installed() {
        let info = info(Path::new("/nonexistent/kuro-test"));
        assert!(info.selected.is_none());
        assert!(!info.is_installed(Quality::Hd));
        // With no saved choice and nothing on disk, HD is the safe default.
        assert_eq!(info.default_choice(), Quality::Hd);
    }
}
