//! Steam + Proton auto-detection, so installs land where your launcher can
//! find them (e.g. `steamapps/common/...` for a Steam non-game shortcut).
//!
//! Nothing here assumes a layout: a Steam root is recognised by the client it
//! holds, a Proton build by the `proton` script it ships, and a launcher by an
//! `umu-run` — all found by walking `$HOME`, not by trying known locations.
//! Two env overrides skip the walk: `STEAM_ROOT`, `KURO_PROTON` (see
//! [`crate::launch`]).

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

/// Detect a Steam installation: `STEAM_ROOT` if set, else walk `$HOME` for a
/// directory holding the Steam client. No location is assumed — a root is
/// recognised by what it contains, so native, flatpak and relocated installs
/// are all found the same way, and a Wine prefix's decoy `steamapps/` is not.
pub fn detect_steam() -> Option<SteamInfo> {
    if let Some(root) = std::env::var_os("STEAM_ROOT") {
        let root = PathBuf::from(root);
        if root.join("steamapps").is_dir() {
            return Some(steam_info(root));
        }
    }
    let home = std::env::var_os("HOME")?;
    walk_for_steam_root(Path::new(&home), SCAN_DEPTH).map(steam_info)
}

/// A directory is a Steam root when it holds the Steam client itself — both a
/// `steamapps/` library *and* `steam.sh`, the launcher script every install
/// (native, flatpak, snap) ships at its root. `steamapps/` alone proves
/// nothing: a Wine prefix under `$HOME` carries a decoy `steamapps/` tree that
/// is not a Steam installation. The script is the truth, exactly as the
/// `proton` binary is for a Proton build.
fn is_steam_root(dir: &Path) -> bool {
    dir.join("steam.sh").is_file() && dir.join("steamapps").is_dir()
}

fn steam_info(steam_root: PathBuf) -> SteamInfo {
    let libraries = parse_libraryfolders(&steam_root);
    let protons = protons_in(&steam_root);
    SteamInfo {
        steam_root,
        libraries,
        protons,
    }
}

/// Depth-first search for the first directory that is a Steam root
/// ([`is_steam_root`]). Shallowest wins, so a relocated library under a Steam
/// root does not shadow the root itself.
fn walk_for_steam_root(dir: &Path, max_depth: usize) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    walk_steam_roots(dir, 0, max_depth, &mut found);
    found
}

fn walk_steam_roots(dir: &Path, depth: usize, max_depth: usize, found: &mut Option<PathBuf>) {
    if depth > max_depth || found.is_some() {
        return;
    }
    if is_steam_root(dir) {
        *found = Some(dir.to_path_buf());
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            walk_steam_roots(&entry.path(), depth + 1, max_depth, found);
            if found.is_some() {
                return;
            }
        }
    }
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

/// Proton builds under one Steam root — `compatibilitytools.d` (community)
/// and `steamapps/common` (Valve's own). Community builds first.
pub fn protons_in(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_proton_dir(&root.join("compatibilitytools.d"), &mut out);
    scan_proton_dir(&root.join("steamapps/common"), &mut out);
    sort_protons(&mut out);
    out
}

/// A Proton build and/or an `umu-run` executable, located on disk.
#[derive(Debug, Clone, Default)]
pub struct Runners {
    /// Directory holding a `proton` script (the build root), if found.
    pub proton: Option<PathBuf>,
    /// An `umu-run` executable, if found.
    pub umu_run: Option<PathBuf>,
}

/// How deep below `$HOME` to look. A Proton build sits several levels down
/// (`.../compatibilitytools.d/<build>/proton`); the margin covers
/// launcher-managed trees without walking the whole home.
const SCAN_DEPTH: usize = 8;

/// Directories that never hold a runner or a Steam root; skipping them keeps
/// the walks quick without changing what they find.
const SKIP_DIRS: &[&str] = &[
    "node_modules", ".git", "target", ".cache", ".cargo", ".rustup", "snap", ".npm",
];

/// Find a Proton build and an `umu-run` by walking `$HOME` — nothing is
/// assumed about where a launcher put them. Cached: the walk touches the disk,
/// so it runs once per process.
pub fn find_runners() -> Runners {
    static CACHE: std::sync::OnceLock<Runners> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| match std::env::var_os("HOME") {
            Some(home) => walk_for_runners(Path::new(&home), SCAN_DEPTH),
            None => Runners::default(),
        })
        .clone()
}

/// Walk `root` looking for a `proton` script and an `umu-run` executable.
/// Takes an explicit root + depth so it can be tested against a fixture tree.
fn walk_for_runners(root: &Path, max_depth: usize) -> Runners {
    let mut protons: Vec<PathBuf> = Vec::new();
    let mut umu_run: Option<PathBuf> = None;
    walk(root, 0, max_depth, &mut protons, &mut umu_run);
    sort_protons(&mut protons);
    Runners {
        proton: protons.into_iter().next(),
        umu_run,
    }
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    protons: &mut Vec<PathBuf>,
    umu_run: &mut Option<PathBuf>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        // Do not follow directory symlinks (`~/.steam/steam` loops back into
        // the tree); the real directories are reached on their own.
        if ft.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            walk(&entry.path(), depth + 1, max_depth, protons, umu_run);
        } else if ft.is_file() || ft.is_symlink() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "proton" {
                protons.push(dir.to_path_buf());
            } else if name == "umu-run" && umu_run.is_none() {
                *umu_run = Some(entry.path());
            }
        }
    }
}

/// Collect Proton build directories from `dir` into `out`.
///
/// A directory qualifies when its name mentions "proton" (any case, so
/// `DW-Proton`, `GE-Proton`, `dwproton`, `Proton 9.0` all match) but is not a
/// `Proton Runtime`, *and* it actually ships the `proton` script. The name is
/// a hint; the binary is the truth — that is what lets a name we did not
/// anticipate still be found, and keeps a stray empty directory out.
fn scan_proton_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let p = entry.path();
        if name.contains("proton") && !name.contains("runtime") && p.join("proton").is_file() {
            out.push(p);
        }
    }
}

/// Community builds (GE-Proton / dwproton / DW-Proton) first — they carry the
/// ACE and DLSS fixes the game needs — then stable Proton.
fn sort_protons(out: &mut Vec<PathBuf>) {
    out.sort_by(|a, b| {
        let community = |p: &PathBuf| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy().to_lowercase();
                    n.contains("ge-proton") || n.contains("dwproton") || n.contains("dw-proton")
                })
                .unwrap_or(false)
        };
        community(b).cmp(&community(a)).then_with(|| a.cmp(b))
    });
    out.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `protons_in` lists Proton builds under one root, keyed on the shipped
    /// `proton` script rather than a name prefix. Regression: `DW-Proton
    /// Latest` (capital `DW-`) was skipped by a prefix matcher.
    #[test]
    fn protons_in_finds_builds_by_binary_not_name_prefix() {
        let root = std::env::temp_dir().join(format!("kuro-steam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tools = root.join("compatibilitytools.d");
        for dir in ["DW-Proton Latest", "GE-Proton9-1", "Proton Runtime 3.0", "Empty"] {
            std::fs::create_dir_all(tools.join(dir)).unwrap();
        }
        // Only real builds ship `proton`.
        for dir in ["DW-Proton Latest", "GE-Proton9-1", "Proton Runtime 3.0"] {
            std::fs::write(tools.join(dir).join("proton"), b"#!/bin/sh\n").unwrap();
        }

        let found = protons_in(&root);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.contains(&"DW-Proton Latest".to_string()));
        assert!(names.contains(&"GE-Proton9-1".to_string()));
        // A `Proton Runtime` is not a Proton build even though it has a file.
        assert!(!names.contains(&"Proton Runtime 3.0".to_string()));
        // No `proton` binary → not a runner.
        assert!(!names.contains(&"Empty".to_string()));
        // Community builds sort ahead of stable Proton.
        assert_eq!(names.first().map(String::as_str), Some("DW-Proton Latest"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The HOME walk finds a runner wherever it lives — no path is assumed.
    #[test]
    fn walk_finds_proton_and_umu_run_anywhere() {
        let home = std::env::temp_dir().join(format!("kuro-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // A Proton build buried a few levels down, under made-up directory
        // names — the walk knows no layout, so the names must not matter.
        let build = home.join("q/w/e/r/t/y");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("proton"), b"#!/bin/sh\n").unwrap();
        // An umu-run in an unrelated tree.
        let launcher = home.join("x/y");
        std::fs::create_dir_all(&launcher).unwrap();
        std::fs::write(launcher.join("umu-run"), b"#!/bin/sh\n").unwrap();

        let found = walk_for_runners(&home, SCAN_DEPTH);
        assert_eq!(found.proton.as_deref(), Some(build.as_path()));
        assert_eq!(found.umu_run.as_deref(), Some(launcher.join("umu-run").as_path()));

        // The walk is bounded: a runner past the depth limit is not reported.
        let deep = home.join("d/e/f/g/h/i/j/k/l/m");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("proton"), b"#!/bin/sh\n").unwrap();
        let shallow = walk_for_runners(&home, 2);
        assert!(shallow.proton.is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A Steam root is found by the client it contains, at whatever path it
    /// happens to live — the search assumes no location. A bare `steamapps/`
    /// tree is not enough: a Wine prefix's decoy must not be mistaken for one.
    #[test]
    fn walk_finds_steam_root_by_contents() {
        let home = std::env::temp_dir().join(format!("kuro-steamwalk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("somewhere/odd/SteamX");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        std::fs::write(root.join("steam.sh"), b"#!/bin/sh\n").unwrap();

        assert_eq!(walk_for_steam_root(&home, SCAN_DEPTH).as_deref(), Some(root.as_path()));

        // Bounded: the root sits past depth 2, so a shallow walk misses it.
        assert!(walk_for_steam_root(&home, 2).is_none());

        // A `steamapps/` without the client is a Wine-prefix decoy, not a
        // Steam install — with the real root gone, nothing is reported.
        let decoy = home.join("prefix/drive_c/Program Files (x86)/Steam");
        std::fs::create_dir_all(decoy.join("steamapps")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert!(walk_for_steam_root(&home, SCAN_DEPTH).is_none());

        let _ = std::fs::remove_dir_all(&home);
    }
}
