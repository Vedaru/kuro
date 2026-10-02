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
//! A third job lives here too, because it is the same vocabulary: **which
//! bodies a run downloads** ([`body_of`], [`served_bodies`],
//! [`narrow_to_body`]). The full manifest a channel serves carries the base
//! paks plus one or more tier directories — WuWa 3.7.0 ships `Client/Content/HD`
//! at ~42.6 GiB *inside* the same manifest as the base paks — so without a
//! choice kuro fetched every body the channel offered, and there was no way to
//! install just the set the client will mount. The manifest is narrowed once,
//! where it enters [`crate::GameManager`], so verify / repair / sweep all agree
//! on what this install tracks.
//!
//! This whole scheme belongs to the UE titles only — see
//! `Game::uses_quality_tiers`. PGR (Unity) keeps its assets in `PGR_Data/` and
//! takes no tier argument, so the launcher and UI skip quality entirely for it,
//! and its manifests carry no bodies to narrow to.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use kuro_api::{Error, PatchIndex, Result};

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
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&SavedQuality { quality: q })?,
    )?;
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

/// Which pak body a manifest entry belongs to — `Some` only for the three tier
/// directories, i.e. `Client/Content/<SD|HD|UHD>/…`.
///
/// Everything else (`Client/Content/Paks` — the base chunk paks —, the
/// binaries under `Client/Binaries`, `Engine/`) is part of *every* install and
/// maps to `None`. The directory segment is matched against the client's own
/// spellings exactly: a manifest that spells a tier some other way is not a
/// body this can narrow to, and is left alone rather than guessed at.
pub fn body_of(dest: &str) -> Option<Quality> {
    let rest = dest
        .trim_start_matches('/')
        .strip_prefix("Client/Content/")?;
    match rest.split('/').next()? {
        "SD" => Some(Quality::Sd),
        "HD" => Some(Quality::Hd),
        "UHD" => Some(Quality::Uhd),
        _ => None,
    }
}

/// The bodies a manifest actually serves, cheapest first.
///
/// A body counts as served when the manifest carries at least one entry under
/// its directory. The channels checked on 2026-10 (WuWa CN `G152` and Global
/// `G153`, both v3.7.0) serve `Client/Content/Paks` plus a single `HD` body, so
/// this is what tells a caller whether narrowing is meaningful at all.
pub fn served_bodies(index: &PatchIndex) -> Vec<Quality> {
    Quality::ALL
        .into_iter()
        .filter(|q| index.resource.iter().any(|r| body_of(&r.dest) == Some(*q)))
        .collect()
}

/// Narrow a manifest to one body, in place.
///
/// `want = None` leaves the manifest exactly as served — the behaviour when no
/// preset is saved. `Some(q)` keeps every entry outside the bodies plus `q`'s
/// own entries, and drops the rest; the dropped count comes back so the caller
/// can say what it is *not* downloading.
///
/// `strict` decides what happens when the channel serves no `q` at all: an
/// explicit `--quality sd` against an HD-only channel is a user mistake and is
/// answered with an error naming what the channel does serve, while a repair
/// run applying a *saved* preset must not die because that preset is currently
/// unserved — it gets the whole manifest instead.
///
/// `deleteFiles` and the krpdiff group members are paths too, so they are
/// filtered alongside `resource`: a narrowed manifest must not ask the engine
/// to delete or merge files it no longer describes.
pub fn narrow_to_body(
    index: &mut PatchIndex,
    want: Option<Quality>,
    strict: bool,
) -> Result<usize> {
    let Some(q) = want else {
        return Ok(0);
    };
    if !index.resource.iter().any(|r| body_of(&r.dest) == Some(q)) {
        if !strict {
            return Ok(0);
        }
        let served = served_bodies(index);
        let list = if served.is_empty() {
            "none".to_string()
        } else {
            served
                .iter()
                .map(|s| s.as_arg().to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(Error::Patch(format!(
            "this channel's manifest serves no {} body (it serves: {list}) — drop --quality, or pick one of those",
            q.as_arg()
        )));
    }

    let keep = |dest: &str| match body_of(dest) {
        Some(b) => b == q,
        None => true,
    };
    let before = index.resource.len();
    index.resource.retain(|r| keep(&r.dest));
    index.delete_files.retain(|d| keep(d));
    for group in index.group_infos.iter_mut() {
        group.src_files.retain(|f| keep(&f.dest));
        group.dst_files.retain(|f| keep(&f.dest));
    }
    Ok(before - index.resource.len())
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

    // ---- download bodies ------------------------------------------------

    fn res(dest: &str) -> kuro_api::ResourceItem {
        kuro_api::ResourceItem {
            dest: dest.to_string(),
            md5: String::new(),
            size: 1,
            from_folder: None,
            chunk_infos: vec![],
        }
    }

    fn file_ref(dest: &str) -> kuro_api::FileRef {
        kuro_api::FileRef {
            dest: dest.to_string(),
            md5: String::new(),
            size: 1,
            chunk_infos: vec![],
        }
    }

    /// A manifest of the shape a channel serves: base paks + the given bodies.
    fn manifest(bodies: &[Quality]) -> PatchIndex {
        let mut dests = vec![
            "Client/Binaries/Win64/Client-Win64-Shipping.exe".to_string(),
            "Client/Content/Paks/pakchunk0.pak".to_string(),
        ];
        for q in bodies {
            dests.push(format!("Client/Content/{}/body.pak", q.dir_name()));
        }
        PatchIndex {
            resource: dests.iter().map(|d| res(d)).collect(),
            delete_files: vec!["Client/Content/HD/gone.pak".to_string()],
            group_infos: vec![kuro_api::GroupInfo {
                dest: "group0.krpdiff".to_string(),
                src_files: vec![file_ref("Client/Content/HD/old.pak")],
                dst_files: vec![file_ref("Client/Content/HD/new.pak")],
            }],
            apply_types: vec![],
        }
    }

    #[test]
    fn body_of_matches_only_the_tier_directories() {
        assert_eq!(body_of("Client/Content/HD/x.pak"), Some(Quality::Hd));
        assert_eq!(body_of("/Client/Content/UHD/a/b.pak"), Some(Quality::Uhd));
        assert_eq!(body_of("Client/Content/SD/y.pak"), Some(Quality::Sd));
        // Not bodies: the base chunk paks, a prefix lookalike, anything else.
        assert_eq!(body_of("Client/Content/Paks/x.pak"), None);
        assert_eq!(body_of("Client/Content/HDD/x.pak"), None);
        assert_eq!(body_of("Client/Binaries/Win64/PGR.exe"), None);
        // Purely a path rule — a bare directory path is still that body, which
        // is what `deleteFiles` entries look like when they name one.
        assert_eq!(body_of("Client/Content/HD"), Some(Quality::Hd));
    }

    #[test]
    fn served_bodies_reports_what_the_channel_carries() {
        // The shape the WuWa channels serve today: base paks + a single HD body.
        assert_eq!(served_bodies(&manifest(&[Quality::Hd])), vec![Quality::Hd]);
        assert_eq!(
            served_bodies(&manifest(&[Quality::Uhd, Quality::Hd])),
            vec![Quality::Hd, Quality::Uhd]
        );
        assert!(served_bodies(&manifest(&[])).is_empty());
    }

    #[test]
    fn narrowing_keeps_the_base_files_and_the_chosen_body() {
        let mut index = manifest(&[Quality::Hd, Quality::Uhd]);
        let dropped = narrow_to_body(&mut index, Some(Quality::Hd), true).unwrap();
        assert_eq!(dropped, 1);
        let dests: Vec<&str> = index.resource.iter().map(|r| r.dest.as_str()).collect();
        assert!(dests.contains(&"Client/Binaries/Win64/Client-Win64-Shipping.exe"));
        assert!(dests.contains(&"Client/Content/Paks/pakchunk0.pak"));
        assert!(dests.contains(&"Client/Content/HD/body.pak"));
        assert!(!dests.contains(&"Client/Content/UHD/body.pak"));
        // Delete lists and krpdiff group members follow the same rule.
        assert_eq!(
            index.delete_files,
            vec!["Client/Content/HD/gone.pak".to_string()]
        );
        assert_eq!(index.group_infos[0].dst_files.len(), 1);
    }

    #[test]
    fn narrowing_to_an_unserved_body_is_loud_when_asked_for() {
        let mut index = manifest(&[Quality::Hd]);
        let err = narrow_to_body(&mut index, Some(Quality::Sd), true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("serves no SD body"), "{msg}");
        assert!(msg.contains("it serves: hd"), "{msg}");
        // …and the manifest is untouched by the failed attempt.
        assert_eq!(index.resource.len(), 3);
    }

    #[test]
    fn a_saved_preset_that_is_not_served_falls_back_to_everything() {
        // A repair run must not die because a saved preset is unserved.
        let mut index = manifest(&[Quality::Hd]);
        assert_eq!(
            narrow_to_body(&mut index, Some(Quality::Uhd), false).unwrap(),
            0
        );
        assert_eq!(index.resource.len(), 3);
    }

    #[test]
    fn no_choice_leaves_the_manifest_exactly_as_served() {
        let mut index = manifest(&[Quality::Hd, Quality::Uhd]);
        assert_eq!(narrow_to_body(&mut index, None, true).unwrap(), 0);
        assert_eq!(index.resource.len(), 4);
    }
}
