//! Steam + Proton auto-detection, so installs land where your launcher can
//! find them (e.g. `steamapps/common/...` for a Steam non-game shortcut).

use std::path::{Path, PathBuf};

use kuro_api::Game;

#[derive(Debug, Clone)]
pub struct SteamInfo {
    /// Steam install root (e.g. `~/.local/share/Steam`).
    pub steam_root: PathBuf,
    /// `steamapps` directories (primary + extra libraries).
    pub libraries: Vec<PathBuf>,
    /// Proton versions found (compatibilitytools.d + steamapps/common).
    pub protons: Vec<PathBuf>,
}

impl SteamInfo {
    /// First detected Proton (GE-Proton preferred), if any.
    pub fn proton(&self) -> Option<&Path> {
        self.protons.first().map(|p| p.as_path())
    }
}

/// Detect a Steam installation in the usual places (native, flatpak, env).
pub fn detect_steam() -> Option<SteamInfo> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("STEAM_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    candidates.push(PathBuf::from(&home).join(".local/share/Steam"));
    candidates.push(PathBuf::from(&home).join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
    candidates.push(PathBuf::from(&home).join(".steam/steam")); // legacy symlink

    for root in candidates {
        if !root.join("steamapps").is_dir() {
            continue;
        }
        let libraries = parse_libraryfolders(&root);
        let protons = find_protons(&root);
        return Some(SteamInfo {
            steam_root: root.clone(),
            libraries,
            protons,
        });
    }
    None
}

/// Default install location for a game inside the primary Steam library.
pub fn default_game_dir(steam: &SteamInfo, game: Game) -> PathBuf {
    let name = match game {
        Game::WuWa => "Wuthering Waves",
        Game::Pgr => "Punishing Gray Raven",
    };
    let steamapps = steam
        .libraries
        .first()
        .cloned()
        .unwrap_or_else(|| steam.steam_root.join("steamapps"));
    steamapps.join("common").join(name)
}

fn parse_libraryfolders(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.join("steamapps")];
    let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) else {
        return libs;
    };
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('"') {
            continue;
        }
        let mut parts = t[1..].splitn(2, '"');
        if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
            if key == "path" {
                libs.push(PathBuf::from(val.trim_matches('"')).join("steamapps"));
            }
        }
    }
    libs
}

fn find_protons(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in [
        root.join("compatibilitytools.d"),
        root.join("steamapps/common"),
    ] {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_proton = name.starts_with("GE-Proton")
                || name.starts_with("dwproton")
                || (name.starts_with("Proton") && !name.contains("Runtime"));
            if is_proton && p.is_dir() {
                out.push(p);
            }
        }
    }
    // GE-Proton / dwproton first, then stable Proton
    out.sort_by(|a, b| {
        let a_ge = a.file_name().map(|n| n.to_string_lossy().starts_with("GE-Proton") || n.to_string_lossy().starts_with("dwproton"));
        let b_ge = b.file_name().map(|n| n.to_string_lossy().starts_with("GE-Proton") || n.to_string_lossy().starts_with("dwproton"));
        b_ge.cmp(&a_ge).then_with(|| a.cmp(b))
    });
    out
}
