//! `kuro` — ratatui terminal UI.
//!
//! Two screens: a games list, then a per-game detail view. `Esc` backs up one
//! level, so exactly one game is ever on screen. Keys on the detail screen:
//! `r` refresh · `p` play · `d` update · `a` apply · `s` sync · `c` checkout
//! CN<->bilibili · `Q` quality preset · `q` quit.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{init, restore, Frame, Terminal};

use kuro_core::{
    default_game_dir, detect_game, detect_steam, launch, quality, BodyChoice, Game, GameManager, GameStatus,
    ProgressEvent, Quality, QualityInfo, Server, SteamInfo,
};

/// Where WuWa lives when no folder is given on the command line: the first
/// Steam library's `common/` if Steam is installed (which is where a Proton
/// install of this game actually lands), else `~/Games/Wuthering Waves`.
///
/// Resolved at runtime rather than compiled in — a hardcoded `$HOME` is only
/// correct on the machine it was written on, and this is the path every bare
/// `kuro play` / `kuro sync` falls back to.
fn default_folder() -> String {
    let steam = detect_steam();
    match &steam {
        Some(info) => default_game_dir(info, Game::WuWa).to_string_lossy().into_owned(),
        None => std::env::var("HOME")
            .map(|home| {
                PathBuf::from(home)
                    .join("Games/Wuthering Waves")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default(),
    }
}

enum UiEvent {
    /// Status result for one game (index into `UiState::statuses`).
    Status(usize, Result<GameStatus, String>),
    Progress(ProgressEvent),
    TaskDone(Result<String, String>),
}

#[derive(Default)]
struct TaskUi {
    kind: String,
    done: usize,
    /// Items announced but not yet finished (GroupStart - GroupDone).
    queued: usize,
    finished: Option<Result<String, String>>,
    total_bytes: u64,
    done_bytes: u64,
    /// In-flight files (parallel downloads), each with its own bar.
    files: Vec<FileState>,
}

#[derive(Clone, Default)]
struct FileState {
    name: String,
    done: u64,
    total: u64,
}

/// Which section the Tab-focus is on; highlighted border + section-scoped keys
/// (PgUp/PgDn scroll the log only when it is focused).
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Focus {
    #[default]
    Status,
    Task,
    Log,
}

/// The drill-down stack: the games list is the root, a game detail is one
/// level deep. `Esc` pops, so the other games are never rendered beside the
/// active one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Screen {
    #[default]
    Games,
    Detail,
}

#[derive(Default)]
struct UiState {
    screen: Screen,
    logs: Vec<String>,
    task: Option<TaskUi>,
    busy: bool,
    /// Game folders in the manager; ←/→ switches between them.
    paths: Vec<String>,
    active: usize,
    /// Focused section (Tab cycles; PgUp/PgDn only scroll the log when focused).
    focus: Focus,
    /// Log panel scroll offset (lines from the newest).
    log_scroll: usize,
    /// Open install modal (game + server selection).
    install: Option<InstallDraft>,
    /// Help overlay open.
    show_help: bool,
    /// Detected Steam + Proton (for install targets).
    steam: Option<SteamInfo>,
    /// Cached status per game path.
    statuses: Vec<Option<Result<GameStatus, String>>>,
    /// Saved quality preset per game path (mirrors `.kuro_cache/quality.json`).
    quality_sel: Vec<Option<Quality>>,
    /// Open quality-preset modal.
    quality_modal: Option<QualityDraft>,
}

/// In-progress quality selection: what to write, plus the install's packs.
#[derive(Clone)]
struct QualityDraft {
    game: Game,
    choice: Quality,
    info: QualityInfo,
}

/// In-progress install selection.
#[derive(Clone)]
struct InstallDraft {
    game: Game,
    server: Server,
    /// Install target folder (editable).
    target: String,
    /// True while typing the target path.
    edit_target: bool,
    /// Which quality pack's body to fetch. WuWa only — PGR (Unity) has no
    /// `Client/Content/<TIER>` bodies, so it stays on `All`.
    body: BodyChoice,
}

impl InstallDraft {
    fn new(target: String) -> Self {
        Self {
            game: Game::WuWa,
            server: Server::Cn,
            target,
            edit_target: false,
            body: BodyChoice::All,
        }
    }

    /// Cycle the pack this install fetches: all → sd → hd → uhd → all.
    ///
    /// PGR has no bodies at all, so there is nothing to cycle and the choice
    /// stays `All` — picking one would only earn a manifest that cannot serve
    /// it (see `body_choice`).
    fn cycle_body(&mut self) {
        if !self.game.uses_quality_tiers() {
            self.body = BodyChoice::All;
            return;
        }
        self.body = match self.body {
            BodyChoice::All | BodyChoice::Prefer(_) => BodyChoice::Only(Quality::Sd),
            BodyChoice::Only(Quality::Sd) => BodyChoice::Only(Quality::Hd),
            BodyChoice::Only(Quality::Hd) => BodyChoice::Only(Quality::Uhd),
            BodyChoice::Only(Quality::Uhd) => BodyChoice::All,
        };
    }
}

/// How a body choice reads in the UI.
fn body_label(body: BodyChoice) -> String {
    match body {
        BodyChoice::All => "all (whatever the channel serves)".to_string(),
        BodyChoice::Only(q) | BodyChoice::Prefer(q) => q.as_arg().to_ascii_lowercase(),
    }
}

fn push_log(state: &mut UiState, msg: impl Into<String>) {
    let msg = msg.into();
    state.logs.push(msg);
    if state.logs.len() > 200 {
        state.logs.drain(0..state.logs.len() - 200);
    }
}

/// Human-friendly game name for logs.
fn pretty_game(game: Game) -> &'static str {
    match game {
        Game::WuWa => "Wuthering Waves",
        Game::Pgr => "Punishing: Gray Raven",
    }
}

/// Turn known raw errors into friendly, human-readable messages.
fn friendly_error(raw: &str) -> String {
    if raw.contains("no channel files could be swapped") || raw.contains("checkout:") {
        "checkout isn't possible for this server — its channel files aren't in the manifest; your install is unchanged"
            .to_string()
    } else if raw.contains("patchConfig entry") {
        "already on the latest version — nothing to do".to_string()
    } else if raw.contains("run predownload first") || raw.contains("incremental_download") {
        "no downloaded update found — press 'd' first to download it".to_string()
    } else if raw.contains("NoLocalConfig") || raw.contains("launcher config not found") {
        "no game found in this folder (launcherDownloadConfig.json missing)".to_string()
    } else if raw.contains("global fast-switch") {
        "the global server can't be fast-switched — use 's' to sync instead".to_string()
    } else if raw.contains("already up to date") {
        "already up to date".to_string()
    } else {
        raw.to_string()
    }
}

/// jemalloc instead of the system allocator.
///
/// A sync runs 60+ threads: tokio workers, the `tokio::fs` blocking pool and
/// rayon during verify. glibc gives each thread its own malloc arena, and the
/// download path's short-lived buffers stranded freed pages in them: a measured
/// sync created 69 arenas and grew RSS ~1.9 MB/hour. jemalloc shares arenas and
/// returns unused pages on its default 10 s decay, so RSS settles to a working
/// set. Measured on the same sync: 214 MB flat (204-228) instead of 360 MB and
/// climbing, at the same 3.7 MB/s. Zero decay held 163 MB but spent ~10% more
/// CPU; the default keeps the power cost at the glibc baseline.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // one-shot CLI subcommands
    match args.first().map(|s| s.as_str()) {
        // Answer these before touching the terminal: without a TTY the TUI can
        // neither init nor read a keypress to quit, so falling through here is
        // a hard panic instead of help.
        Some("--help" | "-h" | "help") => {
            print_usage();
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("kuro {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("install") => {
            return cli_install(&args).await;
        }
        Some("status") => {
            let folder = args.get(1).cloned().unwrap_or_else(default_folder);
            return cli_status(&folder).await;
        }
        Some("sync") => {
            return cli_sync(&args).await;
        }
        Some("quality") => {
            return cli_quality(&args);
        }
        Some("pack") => {
            return cli_pack(&args).await;
        }
        Some("update") => {
            return cli_update(&args).await;
        }
        Some("play") => {
            return cli_play(&args);
        }
        Some("kill") => {
            return cli_kill(&args);
        }
        _ => {}
    }

    // TUI: one or more game folders (default: auto-detect installed games)
    let mut paths = args;
    if paths.is_empty() {
        paths = auto_detect_games();
    }
    if paths.is_empty() {
        paths.push(default_folder());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<UiEvent>(64);

    // initial status for every game
    for (idx, path) in paths.iter().enumerate() {
        let tx = tx.clone();
        let path = path.clone();
        tokio::spawn(async move {
            let result = match GameManager::open(PathBuf::from(path)).await {
                Ok(m) => m.status().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiEvent::Status(idx, result)).await;
        });
    }

    let terminal = init();
    let res = run(terminal, &mut rx, tx, paths).await;
    restore();
    res
}

fn print_usage() {
    println!(
        "kuro {ver} — Kuro Games launcher (Wuthering Waves / Punishing: Gray Raven)

usage: kuro [game-folder ...]            launch the TUI (default: auto-detect)
       kuro install <wuwa|pgr> <cn|bilibili|global> <folder> [--quality sd|hd|uhd]
       kuro status [folder]
       kuro sync [folder] [--quality sd|hd|uhd]
       kuro quality <folder> [sd|hd|uhd]
       kuro pack [folder] <sd|hd|uhd> [--check]   install a quality pack
       kuro update [folder] [--check]            apply common + tier patchConfig deltas
       kuro play <folder> [--quality <sd|hd|uhd>]
       kuro kill <folder>
       kuro help | --help | -h
       kuro --version | -V",
        ver = env!("CARGO_PKG_VERSION")
    );
}

/// Resolve `--quality <sd|hd|uhd>` (or the bare positional form) into the body
/// choice an install / sync should make.
///
/// Games without quality packs (PGR) have no `Client/Content/<TIER>` bodies at
/// all, so asking for one there is a mistake rather than a no-op: say so and
/// stop, instead of quietly downloading the whole client.
fn body_choice(game: Option<Game>, rest: &[String]) -> Result<BodyChoice, String> {
    let asked = parse_quality_arg(rest)?;
    let Some(raw) = asked else {
        return Ok(BodyChoice::All);
    };
    if game.is_some_and(|g| !g.uses_quality_tiers()) {
        let name = game.map(pretty_game).unwrap_or("this game");
        return Err(format!(
            "{name} has no quality packs — drop --quality (its manifest has no Client/Content/<SD|HD|UHD> bodies)"
        ));
    }
    match Quality::parse(raw) {
        Some(q) => Ok(BodyChoice::Only(q)),
        None => Err(format!("unknown quality `{raw}` (use sd|hd|uhd)")),
    }
}

async fn cli_install(args: &[String]) -> std::io::Result<()> {
    let game = match args.get(1).map(|s| s.as_str()) {
        Some("wuwa") => Game::WuWa,
        Some("pgr") => Game::Pgr,
        _ => {
            println!("usage: kuro install <wuwa|pgr> <cn|bilibili|global> <folder>");
            return Ok(());
        }
    };
    let server = match args.get(2).map(|s| s.as_str()) {
        Some("cn") => Server::Cn,
        Some("bilibili") => Server::Bilibili,
        Some("global") => Server::Global,
        _ => {
            println!("bad server (cn|bilibili|global)");
            return Ok(());
        }
    };
    let Some(folder) = args.get(3) else {
        println!("missing folder");
        return Ok(());
    };
    let body = match body_choice(Some(game), args.get(4..).unwrap_or(&[])) {
        Ok(b) => b,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };

    match GameManager::install_with_progress(game, server, folder.into(), body, None).await {
        Ok(r) => println!(
            "installed v{}: checked={} ok={} repaired={} failed={}",
            r.version,
            r.sync.checked,
            r.sync.ok,
            r.sync.repaired,
            r.sync.failed.len()
        ),
        Err(e) => println!("install failed: {e}"),
    }
    Ok(())
}

async fn cli_status(folder: &str) -> std::io::Result<()> {
    match GameManager::open(PathBuf::from(folder)).await {
        Ok(m) => match m.status().await {
            Ok(s) => println!(
                "game={} server={} local={:?} remote={} update_available={}",
                s.game, s.server, s.local_version, s.remote_version, s.update_available
            ),
            Err(e) => println!("status error: {e}"),
        },
        Err(e) => println!("open error: {e}"),
    }
    Ok(())
}

/// `kuro sync [folder] [--quality <sd|hd|uhd>]` — verify + repair, optionally
/// against a body other than the saved preset's.
///
/// With no flag the saved preset stands as the body this install tracks
/// (`BodyChoice::Prefer` from `open`, so an unserved preset falls back to
/// everything); an explicit `--quality` replaces it for this run and, being
/// explicit, fails loudly when the channel does not serve that body.
async fn cli_sync(args: &[String]) -> std::io::Result<()> {
    let (folder, rest) = split_play_args(args);
    // Reject a malformed flag before touching the disk, then validate the tier
    // against the game actually in the folder: `body_choice` needs the game to
    // refuse a tier for a title that has none, and PGR has no packs at all, so
    // asking for `--quality hd` on a PGR install has to be an error rather
    // than a silently-ignored flag.
    if let Err(msg) = parse_quality_arg(&rest) {
        println!("{msg}");
        return Ok(());
    }
    match GameManager::open(PathBuf::from(&folder)).await {
        Ok(mut m) => {
            let body = match body_choice(Some(m.game), &rest) {
                Ok(b) => b,
                Err(msg) => {
                    println!("{msg}");
                    return Ok(());
                }
            };
            if body != BodyChoice::All {
                m.set_body(body);
            }
            match m.sync().await {
                Ok(r) => println!(
                    "checked={} ok={} repaired={} stale_removed={} failed={}",
                    r.checked,
                    r.ok,
                    r.repaired,
                    r.orphans_removed,
                    r.failed.len()
                ),
                Err(e) => println!("sync error: {e}"),
            }
        }
        Err(e) => println!("open error: {e}"),
    }
    Ok(())
}

/// `kuro quality [folder] [sd|hd|uhd]` — with no preset, print the current
/// choice, what is on disk and the prefix it will launch in; with a preset,
/// save it.
fn cli_quality(args: &[String]) -> std::io::Result<()> {
    let folder = args.get(1).cloned().unwrap_or_else(default_folder);
    let path = std::path::Path::new(&folder);
    let info = quality::info(path);

    let preset = match args.get(2) {
        Some(s) => match Quality::parse(s) {
            Some(q) => Some(q),
            None => {
                println!("unknown quality `{s}` (use sd|hd|uhd)");
                return Ok(());
            }
        },
        None => None,
    };

    let Some(preset) = preset else {
        println!("folder:   {folder}");
        println!(
            "selected: {}",
            info.selected
                .map(|q| q.as_arg().to_string())
                .unwrap_or_else(|| "none".to_string())
        );
        for q in Quality::ALL {
            // Files on disk and the launcher's records are two different
            // questions: a tier the launcher does not know about offers
            // "repair game" even with every pak present, so both are shown.
            let state = kuro_core::pack::state_of(path, q);
            match info.pack(q) {
                Some(p) if p.present => println!(
                    "  {:<3} {:<9} {} paks, {}{}",
                    q.dir_name(),
                    q.label(),
                    p.files,
                    fmt_bytes(p.bytes),
                    if state.registered() {
                        ", launcher: registered".to_string()
                    } else if state.marker.is_some() || state.in_bundles {
                        ", launcher: partially recorded (run `kuro pack` to fix)".to_string()
                    } else {
                        String::new()
                    }
                ),
                _ => println!("  {:<3} {:<9} not installed", q.dir_name(), q.label()),
            }
        }
        println!("prefix:   {}", launch::resolve_prefix(path).display());
        println!("\nusage: kuro quality <folder> <sd|hd|uhd>  to set");
        println!("       kuro pack [folder] <sd|hd|uhd>       to install the paks");
        println!("       kuro play [folder] [sd|hd|uhd]       to launch");
        return Ok(());
    };

    match quality::set_selected(path, preset) {
        Ok(()) => println!(
            "selected {} — launch with `kuro play \"{folder}\" {}`",
            preset.as_arg(),
            preset.as_arg().to_ascii_lowercase()
        ),
        Err(e) => println!("could not save preset: {e}"),
    }
    Ok(())
}

/// `kuro update [folder] [--check]` — the official pack-document update.
///
/// `common` and the active tier each apply their own `patchConfig` delta, then
/// the game version is recorded. This is the path that moves 3.7.1 to 3.8.0
/// without downloading a full 42 GiB body. `--check` reports the download
/// without writing anything.
async fn cli_update(args: &[String]) -> std::io::Result<()> {
    let (folder, rest) = split_play_args(args);
    let check_only = rest.iter().any(|a| a == "--check");
    let rest: Vec<String> = rest.into_iter().filter(|a| a != "--check").collect();
    if !rest.is_empty() {
        println!("usage: kuro update [folder] [--check]");
        return Ok(());
    }
    let mgr = match GameManager::open(PathBuf::from(&folder)).await {
        Ok(m) => m,
        Err(e) => {
            println!("open error: {e}");
            return Ok(());
        }
    };
    if !mgr.game.uses_quality_tiers() {
        println!("{} has no quality packs", pretty_game(mgr.game));
        return Ok(());
    }
    let tier = quality::selected(std::path::Path::new(&folder))
        .or_else(|| {
            quality::info(std::path::Path::new(&folder))
                .installed()
                .first()
                .copied()
        })
        .unwrap_or(Quality::Hd);

    if check_only {
        match mgr.pack_update_plan(tier).await {
            Ok(plan) => {
                if plan.parts.is_empty() {
                    println!("up to date at {}", plan.version);
                } else {
                    for part in &plan.parts {
                        println!(
                            "  {:<6} {} -> {}  {} file(s), {}",
                            part.name,
                            part.from_version,
                            part.to_version,
                            part.files,
                            fmt_bytes(part.bytes)
                        );
                    }
                    println!(
                        "update -> {}: {} file(s), {}",
                        plan.version,
                        plan.parts.iter().map(|p| p.files).sum::<usize>(),
                        fmt_bytes(plan.parts.iter().map(|p| p.bytes).sum())
                    );
                }
            }
            Err(e) => println!("update check error: {e}"),
        }
        return Ok(());
    }

    match mgr.pack_update(tier, None).await {
        Ok(report) => {
            if report.parts.is_empty() {
                println!("up to date at {}", report.version);
            } else {
                for part in &report.parts {
                    println!(
                        "  {:<6} {} -> {}  merged {}, skipped {}, swapped {}",
                        part.name,
                        part.from_version,
                        part.to_version,
                        part.report.merged,
                        part.report.skipped,
                        part.report.swapped
                    );
                }
                println!("updated to {}", report.version);
            }
        }
        Err(e) => println!("update error: {e}"),
    }
    Ok(())
}

/// `kuro pack [folder] [--quality <sd|hd|uhd>] [--check]` — install a quality
/// pack from the launcher's pack document, then register it with the launcher.
///
/// This is the only way to get SD or UHD on WuWa 3.7: the channel manifests
/// carry `Client/Content/HD` and nothing else, so `sync --quality sd` has
/// nothing to fetch (see [`kuro_core::pack`]). `--check` verifies without
/// downloading and without writing the launcher records.
async fn cli_pack(args: &[String]) -> std::io::Result<()> {
    let (folder, rest) = split_play_args(args);
    let check_only = rest.iter().any(|a| a == "--check");
    let rest: Vec<String> = rest.into_iter().filter(|a| a != "--check").collect();
    let tier = match parse_quality_arg(&rest) {
        Ok(Some(s)) => match Quality::parse(s) {
            Some(q) => q,
            None => {
                println!("unknown quality `{s}` (use sd|hd|uhd)");
                return Ok(());
            }
        },
        Ok(None) => {
            println!("usage: kuro pack [folder] <sd|hd|uhd> [--check]");
            return Ok(());
        }
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    let mgr = match GameManager::open(PathBuf::from(&folder)).await {
        Ok(m) => m,
        Err(e) => {
            println!("open error: {e}");
            return Ok(());
        }
    };
    if !mgr.game.uses_quality_tiers() {
        println!("{} has no quality packs", pretty_game(mgr.game));
        return Ok(());
    }
    match mgr.sync_pack(tier, !check_only, None).await {
        Ok(report) => {
            for part in &report.parts {
                println!(
                    "  {:<6} {:<9} {} files{}",
                    part.name,
                    part.version,
                    part.files,
                    if part.skipped > 0 {
                        format!(", {} channel-specific skipped", part.skipped)
                    } else {
                        String::new()
                    }
                );
            }
            if check_only {
                if report.broken.is_empty() {
                    println!(
                        "{} -> OK ({} files, {})",
                        tier.as_arg(),
                        report.parts.iter().map(|p| p.files).sum::<usize>(),
                        fmt_bytes(report.bytes())
                    );
                } else {
                    println!(
                        "{} -> {} file(s) need download ({})",
                        tier.as_arg(),
                        report.broken.len(),
                        fmt_bytes(report.bytes())
                    );
                    for b in report.broken.iter().take(5) {
                        println!("  {b}");
                    }
                }
                return Ok(());
            }
            println!(
                "{} installed — {} file(s) written, {}{}",
                tier.as_arg(),
                report.parts.iter().map(|p| p.downloaded).sum::<usize>(),
                fmt_bytes(report.bytes()),
                if report.pruned > 0 {
                    format!(", {} stale file(s) removed", report.pruned)
                } else {
                    String::new()
                }
            );
            if report.registered {
                println!(
                    "launcher records in place — `kuro play \"{folder}\" {}` to start",
                    tier.as_arg()
                );
            } else {
                println!("NOT registered — {} pack file(s) failed:", report.broken.len());
                for b in report.broken.iter().take(10) {
                    println!("  {b}");
                }
            }
        }
        Err(e) => println!("pack error: {e}"),
    }
    Ok(())
}

/// Read an optional quality tier from `play`'s trailing arguments. Both the
/// positional form (`hd`) and the flag forms `--quality hd` and `--quality=hd`
/// are accepted — `--help` advertises the flag, so it has to work, and the
/// positional form is the natural spelling users reach for first.
/// Returns `Err(message)` when `--quality` is given without a value.
fn parse_quality_arg(rest: &[String]) -> Result<Option<&str>, &'static str> {
    let mut found = None;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--quality" {
            match rest.get(i + 1) {
                Some(v) => {
                    found = Some(v.as_str());
                    i += 2;
                }
                None => return Err("--quality needs a value (sd|hd|uhd)"),
            }
        } else if let Some(v) = a.strip_prefix("--quality=") {
            found = Some(v);
            i += 1;
        } else {
            found = Some(a);
            i += 1;
        }
    }
    Ok(found)
}

/// Split a subcommand's arguments into the install folder and the rest (the
/// quality/body flag). Shared by `play` and `sync`. The folder is optional —
/// `kuro play` / `kuro sync` fall back to the default — so this must never
/// index past the end (a bare `args[2..]` panicked on `kuro play`). A leading
/// `--flag` is a flag, not a folder, so `kuro play --quality hd` reads as
/// "default folder, quality hd".
fn split_play_args(args: &[String]) -> (String, Vec<String>) {
    let rest = args.get(1..).unwrap_or(&[]);
    match rest.first() {
        Some(folder) if !folder.starts_with("--") => (folder.clone(), rest[1..].to_vec()),
        _ => (default_folder(), rest.to_vec()),
    }
}

/// `kuro play [folder] [--quality <sd|hd|uhd>]` — start the game with kuro's
/// own launcher. The preset defaults to the saved choice (else what is on
/// disk, else HD); a preset whose paks are not installed is refused rather
/// than mounted empty. Games without tiers (PGR) ignore the preset entirely.
fn cli_play(args: &[String]) -> std::io::Result<()> {
    let (folder, rest) = split_play_args(args);
    let path = std::path::Path::new(&folder);
    let game = detect_game(path).unwrap_or(Game::WuWa);
    let info = quality::info(path);

    let quality_arg = match parse_quality_arg(&rest) {
        Ok(q) => q,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    let quality = match quality_arg {
        Some(s) if game.uses_quality_tiers() => match Quality::parse(s) {
            Some(q) => q,
            None => {
                println!("unknown quality `{s}` (use sd|hd|uhd)");
                return Ok(());
            }
        },
        _ => info.default_choice(),
    };

    if game.uses_quality_tiers() && !info.is_installed(quality) {
        println!(
            "{} paks are not installed in {} — download them or pick another preset",
            quality.as_arg(),
            quality::content_dir(path).join(quality.dir_name()).display()
        );
        return Ok(());
    }

    let plan = match launch::plan(path, game, quality) {
        Ok(p) => p,
        Err(e) => {
            println!("cannot launch: {e}");
            return Ok(());
        }
    };
    println!("{}", plan.describe());
    match launch::spawn(&plan) {
        Ok(pid) => println!("started (pid {pid})"),
        Err(e) => println!("launch failed: {e}"),
    }
    Ok(())
}

/// `kuro kill [folder]` — SIGKILL the game kuro last launched for `folder`.
/// For a client that hangs on shutdown and never lets go of the prefix.
fn cli_kill(args: &[String]) -> std::io::Result<()> {
    let folder = args.get(1).cloned().unwrap_or_else(default_folder);
    match launch::kill(std::path::Path::new(&folder)) {
        Ok(r) if r.was_running => println!("killed game process group {}", r.pid),
        Ok(r) => println!("no running game (group {} already exited)", r.pid),
        Err(e) => println!("kill: {e}"),
    }
    Ok(())
}

async fn run(
    mut terminal: Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    rx: &mut tokio::sync::mpsc::Receiver<UiEvent>,
    tx: tokio::sync::mpsc::Sender<UiEvent>,
    paths: Vec<String>,
) -> std::io::Result<()> {
    let mut state = UiState {
        paths,
        steam: detect_steam(),
        statuses: Vec::new(),
        ..Default::default()
    };
    state.statuses = vec![None; state.paths.len()];
    state.quality_sel = state
        .paths
        .iter()
        .map(|p| quality::selected(std::path::Path::new(p)))
        .collect();

    loop {
        terminal.draw(|f| ui(f, &state))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let path = state.paths[state.active].clone();

                // modal shortcuts take priority
                if state.show_help {
                    match key.code {
                        KeyCode::Char('h') | KeyCode::Char('?') | KeyCode::Esc => state.show_help = false,
                        KeyCode::Char('q') => break,
                        _ => {}
                    }
                    continue;
                }

                // quality-preset modal. Taking the draft out of the state
                // rather than poking at it through the `Option` keeps the key
                // handling free of `unwrap`s and makes the reopen explicit: the
                // draft is put back unless the key closed the modal.
                if let Some(mut draft) = state.quality_modal.take() {
                    let mut close = false;
                    let mut install: Option<Quality> = None;
                    match key.code {
                        KeyCode::Char('s') => draft.choice = Quality::Sd,
                        KeyCode::Char('h') => draft.choice = Quality::Hd,
                        KeyCode::Char('u') => draft.choice = Quality::Uhd,
                        // `i` downloads the highlighted tier's paks without
                        // leaving the modal, so the status column updates in
                        // place and Enter can then apply it.
                        KeyCode::Char('i') => install = Some(draft.choice),
                        KeyCode::Enter => {
                            let choice = draft.choice;
                            // Selecting a tier whose paks are not on disk is the
                            // common case (SD/UHD are not part of any channel
                            // manifest) — install it instead of saving a preset
                            // that `play` will then refuse.
                            let installed = draft.info.is_installed(choice);
                            apply_quality(&mut state, &path, choice);
                            if !installed {
                                install = Some(choice);
                            }
                            close = true;
                        }
                        KeyCode::Esc => close = true,
                        _ => {}
                    }
                    if let Some(tier) = install {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: format!("pack-{}", tier.as_arg().to_ascii_lowercase()),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            push_log(
                                &mut state,
                                format!("installing {} paks — the game must be closed", tier.as_arg()),
                            );
                            spawn_simple(&tx, &path, TaskKind::Pack(tier));
                        } else {
                            push_log(&mut state, "busy — wait for the current task to finish");
                        }
                    }
                    if !close {
                        // The packs on disk may have changed since the modal
                        // opened — `i` starts an install right here, and the
                        // panel must stop claiming a freshly downloaded tier is
                        // "not installed".
                        draft.info = quality::info(std::path::Path::new(&path));
                        state.quality_modal = Some(draft);
                    }
                    continue;
                }

                let mut start_install: Option<(Game, Server, String, BodyChoice)> = None;
                if let Some(draft) = state.install.as_mut() {
                    if draft.edit_target {
                        // typing the target path
                        match key.code {
                            KeyCode::Char(c) => draft.target.push(c),
                            KeyCode::Backspace => {
                                draft.target.pop();
                            }
                            KeyCode::Enter | KeyCode::Esc => draft.edit_target = false,
                            _ => {}
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('w') => draft.game = Game::WuWa,
                        KeyCode::Char('p') => {
                            // PGR has no quality packs: whatever was picked for
                            // WuWa must not follow the selection over, or the
                            // install would ask for a body PGR's manifest cannot
                            // serve and fail before downloading anything.
                            draft.game = Game::Pgr;
                            draft.body = BodyChoice::All;
                        }
                        KeyCode::Char('c') => draft.server = Server::Cn,
                        KeyCode::Char('b') => draft.server = Server::Bilibili,
                        KeyCode::Char('g') => draft.server = Server::Global,
                        KeyCode::Char('f') => draft.cycle_body(),
                        KeyCode::Char('s') => {
                            if let Some(steam) = &state.steam {
                                draft.target = default_game_dir(steam, draft.game).to_string_lossy().into_owned();
                            }
                        }
                        KeyCode::Char('t') => draft.edit_target = true,
                        KeyCode::Enter => {
                            start_install = Some((draft.game, draft.server, draft.target.clone(), draft.body));
                        }
                        KeyCode::Esc => state.install = None,
                        _ => {}
                    }
                    // stay in the modal unless Enter was pressed
                    if start_install.is_none() {
                        continue;
                    }
                }
                if let Some((game, server, target, body)) = start_install {
                    state.install = None;
                    if !state.busy {
                        state.busy = true;
                        state.task = Some(TaskUi {
                            kind: "install".into(),
                            ..Default::default()
                        });
                        push_log(
                            &mut state,
                            match body {
                                BodyChoice::All => {
                                    format!("installing {} ({}) into {target}", pretty_game(game), server)
                                }
                                _ => format!(
                                    "installing {} ({}) into {target} — quality pack {}",
                                    pretty_game(game),
                                    server,
                                    body_label(body)
                                ),
                            },
                        );
                        spawn_install(&tx, &target, game, server, body);
                    }
                }

                if state.screen == Screen::Games {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            state.active = state.active.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            state.active = (state.active + 1).min(state.paths.len().saturating_sub(1));
                        }
                        KeyCode::Enter => {
                            if let Some(path) = state.paths.get(state.active).cloned() {
                                state.screen = Screen::Detail;
                                // The detail is a fresh page: the shared
                                // startup log would otherwise name the other
                                // game on a page that is supposed to show one.
                                let opened = pretty_game(active_game(&state));
                                state.logs.clear();
                                push_log(&mut state, format!("opened {opened}"));
                                if state.statuses.get(state.active).and_then(|s| s.as_ref()).is_none() {
                                    spawn_status(&tx, &path, state.active);
                                }
                            }
                        }
                        KeyCode::Char('i') => {
                            if !state.busy {
                                state.install = Some(InstallDraft::new(default_folder()));
                            }
                        }
                        KeyCode::Char('r') => {
                            if !state.busy {
                                for i in 0..state.paths.len() {
                                    spawn_status(&tx, &state.paths[i], i);
                                }
                                push_log(&mut state, "refreshing all games…");
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Char('?') => state.show_help = true,
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Esc => state.screen = Screen::Games,
                    KeyCode::Char('q') => break,
                    KeyCode::Char('h') | KeyCode::Char('?') => state.show_help = true,
                    KeyCode::Char('p') => {
                        if !state.busy {
                            play(&mut state, &path);
                        }
                    }
                    KeyCode::Char('k') => kill_game(&mut state, &path),
                    KeyCode::Char('Q') => {
                        if !state.busy {
                            let game = active_game(&state);
                            if game.uses_quality_tiers() {
                                state.quality_modal = Some(open_quality_draft(&path, &state));
                            } else {
                                push_log(
                                    &mut state,
                                    format!("{} has no quality tiers — it launches as-is", pretty_game(game)),
                                );
                            }
                        }
                    }
                    KeyCode::Tab => {
                        state.focus = match state.focus {
                            Focus::Status => Focus::Task,
                            Focus::Task => Focus::Log,
                            Focus::Log => Focus::Status,
                        };
                    }
                    KeyCode::Char('r') => {
                        if !state.busy {
                            spawn_status(&tx, &path, state.active);
                        }
                    }
                    KeyCode::Char('d') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: if active_game(&state).uses_quality_tiers() {
                                    "update".into()
                                } else {
                                    "predownload".into()
                                },
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_predownload(&tx, &path);
                        }
                    }
                    KeyCode::Char('a') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "apply".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_simple(&tx, &path, TaskKind::Apply);
                        }
                    }
                    KeyCode::Char('s') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "sync".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_simple(&tx, &path, TaskKind::Sync);
                        }
                    }
                    KeyCode::Char('c') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "checkout".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_simple(&tx, &path, TaskKind::Checkout);
                        }
                    }
                    // Scrolling belongs to the log pane; the guard keeps the
                    // other panes from silently swallowing these keys.
                    KeyCode::Up if state.focus == Focus::Log => state.log_scroll += 1,
                    KeyCode::Down if state.focus == Focus::Log => {
                        state.log_scroll = state.log_scroll.saturating_sub(1);
                    }
                    KeyCode::PageUp if state.focus == Focus::Log => state.log_scroll += 10,
                    KeyCode::PageDown if state.focus == Focus::Log => {
                        state.log_scroll = state.log_scroll.saturating_sub(10);
                    }
                    _ => {}
                }
            }
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                UiEvent::Status(idx, s) => {
                    let line = match &s {
                        Ok(gs) => {
                            let local = gs.local_version.as_deref().unwrap_or("not installed");
                            let state = if gs.update_available {
                                "update available"
                            } else {
                                "up to date"
                            };
                            format!(
                                "status: {} ({}) — local {local}, remote {} · {state}",
                                pretty_game(gs.game),
                                gs.server,
                                gs.remote_version
                            )
                        }
                        Err(e) => format!("status error: {}", friendly_error(e)),
                    };
                    push_log(&mut state, line);
                    if idx < state.statuses.len() {
                        state.statuses[idx] = Some(s);
                    }
                }
                UiEvent::Progress(p) => match p {
                    ProgressEvent::Log(m) => push_log(&mut state, m),
                    ProgressEvent::SetTotal { bytes } => {
                        if let Some(t) = state.task.as_mut() {
                            t.total_bytes = bytes;
                            t.files.clear(); // repair phase starts; drop the verify bar
                        }
                    }
                    ProgressEvent::SetQueued { count } => {
                        if let Some(t) = state.task.as_mut() {
                            t.queued = count;
                        }
                    }
                    ProgressEvent::GroupStart { name: _ } => {
                        if let Some(t) = state.task.as_mut() {
                            // queued for download; a row appears once it starts
                            t.queued += 1;
                        }
                    }
                    ProgressEvent::FileProgress { name, bytes, total } => {
                        if let Some(t) = state.task.as_mut() {
                            match t.files.iter_mut().find(|f| f.name == name) {
                                Some(f) => {
                                    f.done = bytes;
                                    f.total = total;
                                }
                                None => t.files.push(FileState {
                                    name,
                                    done: bytes,
                                    total,
                                }),
                            }
                        }
                    }
                    ProgressEvent::GroupDone { name, bytes } => {
                        if let Some(t) = state.task.as_mut() {
                            t.files.retain(|f| f.name != name);
                            t.done += 1;
                            t.done_bytes += bytes;
                            t.queued = t.queued.saturating_sub(1);
                        }
                    }
                    ProgressEvent::Done => {
                        if let Some(t) = state.task.as_mut() {
                            t.finished = Some(Ok("download complete — press 'a' to apply".into()));
                        }
                        state.busy = false;
                    }
                },
                UiEvent::TaskDone(r) => {
                    match &r {
                        Ok(msg) => push_log(&mut state, msg.clone()),
                        Err(e) => push_log(&mut state, format!("failed: {}", friendly_error(e))),
                    }
                    if let Some(t) = state.task.as_mut() {
                        t.finished = Some(r);
                    }
                    state.busy = false;
                }
            }
        }
    }
    Ok(())
}

fn spawn_status(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str, idx: usize) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let result = match GameManager::open(PathBuf::from(path)).await {
            Ok(m) => m.status().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(UiEvent::Status(idx, result)).await;
    });
}

/// The game behind the active folder, taken from its status (WuWa by default
/// while the status is still loading).
fn active_game(state: &UiState) -> Game {
    state
        .statuses
        .get(state.active)
        .and_then(|s| s.as_ref())
        .and_then(|r| r.as_ref().ok())
        .map(|gs| gs.game)
        .or_else(|| detect_game(std::path::Path::new(&state.paths[state.active])))
        .unwrap_or(Game::WuWa)
}

/// Read the install's packs and saved choice for the modal.
fn open_quality_draft(folder: &str, state: &UiState) -> QualityDraft {
    let p = std::path::Path::new(folder);
    let game = active_game(state);
    let info = quality::info(p);
    let choice = info.default_choice();
    QualityDraft { game, choice, info }
}

/// Save the chosen preset; log it (the writable marker `-krqlv=` is added
/// afresh on every kuro launch, so nothing else needs updating).
fn apply_quality(state: &mut UiState, folder: &str, choice: Quality) {
    match quality::set_selected(std::path::Path::new(folder), choice) {
        Ok(()) => push_log(
            state,
            format!(
                "quality: set {} ({}) — press 'p' to play",
                choice.as_arg(),
                choice.label()
            ),
        ),
        Err(e) => push_log(state, format!("quality save failed: {e}")),
    }
    if let Some(slot) = state.quality_sel.get_mut(state.active) {
        *slot = Some(choice);
    }
}

/// Launch the active game with kuro's built-in launcher ('p'). Uses the saved
/// preset (else what is on disk, else HD) and refuses a preset whose paks are
/// not installed rather than mounting an empty directory. Games without tiers
/// (PGR) skip the preset entirely.
fn play(state: &mut UiState, folder: &str) {
    let p = std::path::Path::new(folder);
    let game = active_game(state);
    let info = quality::info(p);
    let quality = state
        .quality_sel
        .get(state.active)
        .copied()
        .flatten()
        .or(info.selected)
        .unwrap_or_else(|| info.default_choice());
    if game.uses_quality_tiers() && !info.is_installed(quality) {
        push_log(
            state,
            format!("cannot play: {} paks are not installed", quality.as_arg()),
        );
        return;
    }
    match launch::plan(p, game, quality) {
        Ok(plan) => match launch::spawn(&plan) {
            Ok(pid) => push_log(state, format!("launching {} — pid {pid}", plan.brief())),
            Err(e) => push_log(state, format!("launch failed: {e}")),
        },
        Err(e) => push_log(state, format!("cannot launch: {e}")),
    }
}

/// Force-kill the active game ('k') — SIGKILL to the whole Proton/wine/client
/// process group, for a client that hangs on shutdown.
fn kill_game(state: &mut UiState, folder: &str) {
    match launch::kill(std::path::Path::new(folder)) {
        Ok(r) if r.was_running => push_log(state, format!("force-killed game (group {})", r.pid)),
        Ok(r) => push_log(state, format!("no running game (group {} already exited)", r.pid)),
        Err(e) => push_log(state, format!("kill: {e}")),
    }
}

/// Find installed Kuro games: `~/Games` (the non-Steam layout this box uses)
/// plus the standard Steam library folders — anything carrying the official
/// launcher's `launcherDownloadConfig.json` counts as an install.
fn auto_detect_games() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.push(PathBuf::from(&home).join("Games"));
    if let Some(steam) = detect_steam() {
        for lib in &steam.libraries {
            roots.push(lib.join("common"));
        }
    }
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            if dir.is_dir() && dir.join("launcherDownloadConfig.json").is_file() {
                out.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    // Dedupe by CANONICAL path: `~/Games/<Game>` and a symlink alias
    // (`steamapps/common/<Game>` -> `~/Games/<Game>`) are different strings
    // but the same game — string dedup misses them. Keep the first (real,
    // earlier-root) path, drop the symlink alias.
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut deduped: Vec<String> = Vec::new();
    for p in out {
        let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(&p));
        if seen.insert(canon) {
            deduped.push(p);
        }
    }
    // WuWa is the primary title, then PGR, then anything else. The list is
    // still one row per game; this only decides the default cursor.
    deduped.sort_by_key(|p| {
        let lower = p.to_ascii_lowercase();
        let rank = if lower.contains("wuthering") {
            0
        } else if lower.contains("punishing") {
            1
        } else {
            2
        };
        (rank, p.clone())
    });
    deduped
}

fn spawn_predownload(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let result = async {
            let mgr = GameManager::open(PathBuf::from(&path))
                .await
                .map_err(|e| e.to_string())?;
            if mgr.game.uses_quality_tiers() && !mgr.server_entry().pack_config_urls.is_empty() {
                let folder = PathBuf::from(&path);
                let tier = quality::selected(&folder)
                    .or_else(|| quality::info(&folder).installed().first().copied())
                    .unwrap_or(Quality::Hd);
                let (ptx, mut prx) = tokio::sync::mpsc::channel(256);
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = prx.recv().await {
                        let _ = tx2.send(UiEvent::Progress(ev)).await;
                    }
                });
                let report = mgr.pack_update(tier, Some(ptx)).await.map_err(|e| e.to_string())?;
                return Ok::<_, String>(if report.parts.is_empty() {
                    format!("up to date at {}", report.version)
                } else {
                    format!("pack update applied → {}", report.version)
                });
            }
            let plan = mgr.plan_predownload().await.map_err(|e| e.to_string())?;
            let (ptx, mut prx) = tokio::sync::mpsc::channel(256);
            let tx2 = tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = prx.recv().await {
                    let _ = tx2.send(UiEvent::Progress(ev)).await;
                }
            });
            let _ = ptx
                .send(ProgressEvent::SetTotal {
                    bytes: plan.total_bytes,
                })
                .await;
            mgr.predownload(&plan, ptx).await.map_err(|e| e.to_string())?;
            if plan.total_bytes == 0 {
                Ok::<_, String>("already up to date — nothing to download".to_string())
            } else {
                Ok::<_, String>(format!(
                    "predownload complete — {} → {} staged ({} groups, {} files, {:.1} GiB)",
                    plan.from_version,
                    plan.to_version,
                    plan.patch_groups.len(),
                    plan.full_files.len(),
                    plan.total_bytes as f64 / (1 << 30) as f64
                ))
            }
        }
        .await;
        let _ = tx.send(UiEvent::TaskDone(result)).await;
    });
}

fn spawn_install(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str, game: Game, server: Server, body: BodyChoice) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<ProgressEvent>(256);
        let tx2 = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = prx.recv().await {
                let _ = tx2.send(UiEvent::Progress(ev)).await;
            }
        });
        let folder = PathBuf::from(path);
        let result = match GameManager::install_with_progress(game, server, folder, body, Some(ptx)).await {
            Ok(r) => {
                let exe = r.game_exe.map(|e| format!(" — exe: {e}")).unwrap_or_default();
                Ok(format!(
                    "install complete — {} v{} (game files){exe} · press 'p' to play",
                    pretty_game(game),
                    r.version
                ))
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(UiEvent::TaskDone(result)).await;
    });
}

enum TaskKind {
    Apply,
    Sync,
    Checkout,
    /// Install one quality pack (files + launcher records).
    Pack(Quality),
}

fn spawn_simple(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str, kind: TaskKind) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        // progress relay (sync emits per-file progress)
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<ProgressEvent>(256);
        let tx2 = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = prx.recv().await {
                let _ = tx2.send(UiEvent::Progress(ev)).await;
            }
        });
        let mut ptx = Some(ptx);
        let result = async {
            let mgr = GameManager::open(PathBuf::from(path))
                .await
                .map_err(|e| e.to_string())?;
            match kind {
                TaskKind::Apply => {
                    let report = mgr.apply_with_progress(ptx.take()).await.map_err(|e| e.to_string())?;
                    Ok(format!(
                        "apply complete — merged {}, skipped {}, fallback {}, swapped {}, deleted {}",
                        report.merged,
                        report.skipped,
                        report.fallback,
                        report.swapped,
                        report.deleted.len()
                    ))
                }
                TaskKind::Sync => {
                    let report = mgr.sync_with_progress(ptx.take()).await.map_err(|e| e.to_string())?;
                    let failed = if report.failed.is_empty() {
                        String::new()
                    } else {
                        format!(", {} failed", report.failed.len())
                    };
                    Ok(format!(
                        "sync complete — {} repaired ({:.1} GiB), {} ok, {} stale removed{failed}",
                        report.repaired,
                        report.repaired_bytes as f64 / (1 << 30) as f64,
                        report.ok,
                        report.orphans_removed
                    ))
                }
                TaskKind::Pack(tier) => {
                    let report = mgr.sync_pack(tier, true, ptx.take()).await.map_err(|e| e.to_string())?;
                    let failed = report.broken.len();
                    Ok(if report.registered {
                        format!(
                            "{} pack installed — {} file(s), {:.1} GiB, launcher records in place{}{}",
                            tier.as_arg(),
                            report.parts.iter().map(|p| p.downloaded).sum::<usize>(),
                            report.bytes() as f64 / (1 << 30) as f64,
                            if failed > 0 {
                                format!(", {failed} failed")
                            } else {
                                String::new()
                            },
                            if report.pruned > 0 {
                                format!(", {} stale removed", report.pruned)
                            } else {
                                String::new()
                            }
                        )
                    } else {
                        format!(
                            "{} pack incomplete — {failed} file(s) failed, launcher records NOT written",
                            tier.as_arg()
                        )
                    })
                }
                TaskKind::Checkout => {
                    // toggle between cn and bilibili
                    let target = match mgr.server {
                        Server::Cn => Server::Bilibili,
                        _ => Server::Cn,
                    };
                    let report = mgr.checkout(target).await.map_err(|e| e.to_string())?;
                    Ok(format!(
                        "checkout complete — {} → {}, {} files swapped, now v{}",
                        report.from_server, report.to_server, report.swapped_files, report.new_version
                    ))
                }
            }
        }
        .await;
        let _ = tx.send(UiEvent::TaskDone(result)).await;
    });
}

fn ui(f: &mut Frame, state: &UiState) {
    if f.area().width < 60 || f.area().height < 20 {
        draw_too_small(f);
        return;
    }
    match state.screen {
        Screen::Games => ui_games(f, state),
        Screen::Detail => ui_detail(f, state),
    }
    render_overlays(f, state);
}

/// The designed floor. Below this, name the dimensions instead of drawing a
/// broken frame. `q` and `Ctrl+C` still work because the event loop runs.
fn draw_too_small(f: &mut Frame) {
    let area = f.area();
    let msg = vec![
        Line::styled(
            "terminal too small",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(format!("kuro needs 60x20, this is {}x{}", area.width, area.height)),
        Line::raw("resize the terminal, or press q to quit"),
    ];
    f.render_widget(
        Paragraph::new(msg).block(Block::bordered().border_type(BorderType::Rounded).title("kuro")),
        area,
    );
}

fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded).title(title)
}

/// The root screen: one row per installed game, one game per row. The detail
/// view is a separate screen, so two games are never compared side by side.
fn ui_games(f: &mut Frame, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5), // games table
            Constraint::Length(if state.task.is_some() || state.busy { 6 } else { 3 }),
            Constraint::Min(3),    // recent log
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    let wide = f.area().width >= 100;
    let medium = f.area().width >= 80;
    let (header, widths) = if wide {
        (
            Row::new(["GAME", "SERVER", "LOCAL", "REMOTE", "QUALITY", "STATE"])
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
            vec![
                Constraint::Min(18),
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Min(16),
            ],
        )
    } else if medium {
        (
            Row::new(["GAME", "SERVER", "VERSION", "STATE"])
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
            vec![
                Constraint::Min(18),
                Constraint::Length(10),
                Constraint::Length(18),
                Constraint::Min(16),
            ],
        )
    } else {
        (
            Row::new(["GAME", "VERSION", "STATE"])
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
            vec![Constraint::Min(18), Constraint::Length(17), Constraint::Min(14)],
        )
    };

    let rows: Vec<Row> = state
        .paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let status = state.statuses.get(i).and_then(|s| s.as_ref());
            let quality = state.quality_sel.get(i).copied().flatten();
            let name = match status.and_then(|s| s.as_ref().ok()) {
                Some(s) if medium => pretty_game(s.game).to_string(),
                Some(s) => short_game_name(s.game).to_string(),
                None => folder_name(path),
            };
            let (server, local, remote, state_text, state_style) = match status {
                Some(Ok(s)) => (
                    s.server.to_string(),
                    s.local_version.clone().unwrap_or_else(|| "none".into()),
                    s.remote_version.clone(),
                    if s.local_version.is_none() {
                        "not installed"
                    } else if s.update_available {
                        "update available"
                    } else {
                        "up to date"
                    },
                    if s.local_version.is_none() {
                        Style::default().fg(Color::Red)
                    } else if s.update_available {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Green)
                    },
                ),
                Some(Err(_)) => (
                    "?".into(),
                    "?".into(),
                    "?".into(),
                    "status error",
                    Style::default().fg(Color::Red),
                ),
                None => (
                    "?".into(),
                    "loading".into(),
                    "…".into(),
                    "loading…",
                    Style::default().fg(Color::DarkGray),
                ),
            };
            let version = if local == remote {
                local.clone()
            } else {
                format!("{local} → {remote}")
            };
            let quality = if quality.is_some() {
                quality.map(|q| q.as_arg().to_string()).unwrap_or_else(|| "—".into())
            } else {
                "—".into()
            };
            let cells: Vec<Cell> = if wide {
                vec![
                    Cell::from(name),
                    Cell::from(server),
                    Cell::from(local),
                    Cell::from(remote),
                    Cell::from(quality),
                    Cell::from(Span::styled(state_text, state_style)),
                ]
            } else if medium {
                vec![
                    Cell::from(name),
                    Cell::from(server),
                    Cell::from(version),
                    Cell::from(Span::styled(state_text, state_style)),
                ]
            } else {
                vec![
                    Cell::from(name),
                    Cell::from(version),
                    Cell::from(Span::styled(state_text, state_style)),
                ]
            };
            Row::new(cells)
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(format!("kuro · games ({})", state.paths.len())))
        .row_highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    let visible = chunks[0].height.saturating_sub(3) as usize;
    let offset = state.active.saturating_sub(visible.saturating_sub(1));
    let mut ts = TableState::default()
        .with_selected(Some(state.active))
        .with_offset(offset);
    f.render_stateful_widget(table, chunks[0], &mut ts);

    render_task(f, state, chunks[1], false);
    render_log(f, state, chunks[2], false);
    f.render_widget(Paragraph::new(footer(state, f.area().width)), chunks[3]);
}

fn folder_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("game").to_string()
}

fn short_game_name(game: Game) -> &'static str {
    match game {
        Game::WuWa => "WuWa",
        Game::Pgr => "PGR",
    }
}

fn ui_detail(f: &mut Frame, state: &UiState) {
    let folder = state.paths.get(state.active).map(String::as_str).unwrap_or("");
    let status = state.statuses.get(state.active).and_then(|s| s.as_ref());
    let game_name = status
        .and_then(|s| s.as_ref().ok())
        .map(|s| pretty_game(s.game).to_string())
        .unwrap_or_else(|| folder_name(folder));
    let server = status
        .and_then(|s| s.as_ref().ok())
        .map(|s| format!(" › {}", s.server))
        .unwrap_or_default();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // breadcrumb: kuro › games › <game> › <server>
            Constraint::Length(9), // status: the open game only
            Constraint::Length(if state.task.is_some() || state.busy { 9 } else { 4 }),
            Constraint::Min(4),    // log
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("kuro", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(" › games › ", Style::default().fg(Color::DarkGray)),
            Span::styled(game_name, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(server, Style::default().fg(Color::DarkGray)),
            Span::styled("    Esc back", Style::default().fg(Color::DarkGray)),
        ])),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(status_box_lines(
            status,
            state.busy,
            state.quality_sel.get(state.active).copied().flatten(),
        ))
        .wrap(Wrap { trim: true })
        .block(
            panel("status").border_style(Style::default().fg(if state.focus == Focus::Status {
                Color::Cyan
            } else {
                Color::DarkGray
            })),
        ),
        chunks[1],
    );

    render_task(f, state, chunks[2], state.focus == Focus::Task);
    render_log(f, state, chunks[3], state.focus == Focus::Log);
    f.render_widget(Paragraph::new(footer(state, f.area().width)), chunks[4]);
}

fn task_lines(state: &UiState) -> Vec<Line<'_>> {
    match &state.task {
        None => vec![Line::raw("idle")],
        Some(t) => {
            let mut v = vec![
                Line::raw(format!("task: {}", t.kind)),
                Line::raw(format!("files done: {}   queued: {}", t.done, t.queued)),
            ];
            if t.total_bytes > 0 {
                // Include in-flight bytes: `done_bytes` only moves on GroupDone
                // (a whole file), so the overall bar used to sit still for the
                // entire duration of one file and jump when it landed.
                let in_flight: u64 = t.files.iter().map(|f| f.done).sum();
                let shown = (t.done_bytes + in_flight).min(t.total_bytes);
                v.push(Line::raw(bar_line("overall", shown, t.total_bytes, 34)));
            }
            for f in t.files.iter().take(8) {
                let name = shorten(&f.name, 42);
                v.push(Line::raw(format!("  {}", bar_line(&name, f.done, f.total, 24))));
            }
            if let Some(f) = &t.finished {
                v.push(Line::styled(
                    match f {
                        Ok(m) => format!("✔ {m}"),
                        Err(e) => format!("✘ {}", friendly_error(e)),
                    },
                    Style::default().fg(match f {
                        Ok(_) => Color::Green,
                        Err(_) => Color::Red,
                    }),
                ));
            }
            v
        }
    }
}

fn render_task(f: &mut Frame, state: &UiState, area: ratatui::layout::Rect, focused: bool) {
    f.render_widget(
        Paragraph::new(task_lines(state)).wrap(Wrap { trim: true }).block(
            panel("task").border_style(Style::default().fg(if focused { Color::Cyan } else { Color::DarkGray })),
        ),
        area,
    );
}

fn log_lines(state: &UiState) -> Vec<Line<'_>> {
    // Build only the 60 rows on screen. Materialising the whole 200-line window
    // and cloning it every frame was work the Paragraph never sees.
    let max_scroll = state.logs.len().min(200).saturating_sub(1);
    let scroll = state.log_scroll.min(max_scroll);
    state
        .logs
        .iter()
        .rev()
        .take(200)
        .skip(scroll)
        .take(60)
        .map(|l| Line::raw(l.as_str()))
        .collect()
}

fn render_log(f: &mut Frame, state: &UiState, area: ratatui::layout::Rect, focused: bool) {
    f.render_widget(
        Paragraph::new(log_lines(state)).wrap(Wrap { trim: true }).block(
            panel("log (↑/↓ PgUp/PgDn)").border_style(Style::default().fg(if focused {
                Color::Cyan
            } else {
                Color::DarkGray
            })),
        ),
        area,
    );
}

fn footer(state: &UiState, width: u16) -> Line<'static> {
    let mut spans = Vec::new();
    let keys: &[(&str, &str)] = match (state.screen, width < 84) {
        (Screen::Games, false) => &[
            ("↑↓", "select"),
            ("Enter", "open"),
            ("i", "install"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        (Screen::Games, true) => &[("↑↓", "select"), ("Enter", "open"), ("i", "install"), ("q", "quit")],
        (Screen::Detail, false) => &[
            ("Esc", "back"),
            ("p", "play"),
            ("d", "update"),
            ("s", "sync"),
            ("Q", "quality"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        (Screen::Detail, true) => &[
            ("Esc", "back"),
            ("p", "play"),
            ("d", "update"),
            ("s", "sync"),
            ("q", "quit"),
        ],
    };
    for (i, (key, label)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(Color::DarkGray)));
    }
    if state.busy {
        spans.push(Span::styled(
            "   [busy]",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// Modals overlay whichever screen is active. They are shared, so the list and
/// the detail do not grow their own copies.
fn render_overlays(f: &mut Frame, state: &UiState) {
    if state.show_help {
        let help_lines: Vec<Line> = vec![
            Line::raw("kuro — Kuro Games launcher"),
            Line::raw(""),
            Line::raw("  Games  ↑↓ select · Enter open · i install · r refresh · q quit"),
            Line::raw("  Game   Esc back · p play · d update · s sync · a apply"),
            Line::raw("         Q quality · c switch server · k force-kill · r refresh"),
            Line::raw("         Tab focus panels · ↑↓ / PgUp / PgDn scroll the log"),
            Line::raw(""),
            Line::raw("Install dialog"),
            Line::raw("  w/p game · c/b/g server · f pack · t path · s Steam · Enter go"),
            Line::raw(""),
            Line::raw("From a shell"),
            Line::raw("  kuro update FOLDER --check"),
            Line::raw("  kuro sync FOLDER"),
            Line::raw("  kuro pack FOLDER hd"),
            Line::raw(""),
            Line::raw("h / ? / Esc to close"),
        ];
        let area = centered_rect(70, 60, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(help_lines).block(Block::default().borders(Borders::ALL).title("help")),
            area,
        );
    } else if let Some(draft) = state.quality_modal.as_ref() {
        let mut lines: Vec<Line> = vec![
            Line::raw(format!("Quality preset — {}", pretty_game(draft.game))),
            Line::raw(""),
            Line::raw("  Pick the art size kuro mounts at launch."),
            Line::raw(""),
        ];
        for q in Quality::ALL {
            let key = q.as_arg().to_ascii_lowercase();
            let key = key.chars().next().unwrap_or('?');
            let mark = if draft.choice == q { "▶" } else { " " };
            let current = if draft.info.selected == Some(q) {
                "  (current)"
            } else {
                ""
            };
            let status = match draft.info.pack(q) {
                Some(p) if p.present => format!("{} paks, {}", p.files, fmt_bytes(p.bytes)),
                _ => "not installed".to_string(),
            };
            lines.push(Line::styled(
                format!(" {mark} [{key}] {:<9} {:<20}{current}", q.label(), status),
                if draft.choice == q {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                },
            ));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  [s] [h] [u] pick     [i] install",
            Style::default().fg(Color::Cyan),
        ));
        lines.push(Line::styled(
            "  Enter pick + install     Esc close",
            Style::default().fg(Color::Cyan),
        ));
        let area = centered_rect(68, 46, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("quality")),
            area,
        );
    } else if let Some(draft) = state.install.as_ref() {
        let mut modal_lines: Vec<Line> = vec![
            Line::raw("Install a new game"),
            Line::raw(""),
            Line::raw(format!(
                "  game    [w] wuwa  [p] pgr                → {}",
                if matches!(draft.game, Game::WuWa) {
                    "wuwa"
                } else {
                    "pgr"
                }
            )),
            Line::raw(format!(
                "  server  [c] cn  [b] bilibili  [g] global  → {}",
                draft.server
            )),
            Line::raw(if draft.game.uses_quality_tiers() {
                format!(
                    "  pack    [f] all / sd / hd / uhd          → {}",
                    body_label(draft.body)
                )
            } else {
                format!("  pack    n/a — {} has no quality packs", pretty_game(draft.game))
            }),
            Line::raw(if draft.edit_target {
                "  target  [typing — Enter/Esc to stop]"
            } else {
                "  target"
            }),
            Line::raw(format!("          {}", draft.target)),
            Line::raw("          [t] type path    [s] Steam default"),
            Line::raw(""),
        ];
        match &state.steam {
            Some(steam) => modal_lines.push(Line::raw(format!("  Steam:  {}", steam.steam_root.display()))),
            None => modal_lines.push(Line::raw("  Steam:  not detected")),
        }
        modal_lines.push(Line::raw(""));
        modal_lines.push(Line::styled(
            "  Enter install      Esc cancel",
            Style::default().fg(Color::Cyan),
        ));
        modal_lines.push(Line::raw(""));
        modal_lines.push(Line::raw(
            "  Game files only. `all` takes every pack the channel serves; a single",
        ));
        modal_lines.push(Line::raw(
            "  pack (sd/hd/uhd) fetches just that body — the channel must serve it.",
        ));
        let area = centered_rect(70, 48, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(modal_lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("install")),
            area,
        );
    }
}

/// Lines for a per-game status box. `busy` marks the active game's box while
/// a task is running, so "up to date" never lies about in-flight work.
fn status_box_lines(s: Option<&Result<GameStatus, String>>, busy: bool, quality: Option<Quality>) -> Vec<Line<'_>> {
    match s {
        Some(Ok(s)) => {
            let mut lines = vec![
                Line::raw(format!("game:    {}", s.game)),
                Line::raw(format!("server:  {}", s.server)),
                Line::raw(format!("local:   {}", s.local_version.as_deref().unwrap_or("none"))),
                Line::raw(format!("remote:  {}", s.remote_version)),
            ];
            // PGR (Unity) takes no `-krqlv=` tier, so it gets no quality line.
            if s.game.uses_quality_tiers() {
                lines.push(Line::raw(format!(
                    "quality: {}",
                    quality
                        .map(|q| format!("{} ({})", q.as_arg(), q.label()))
                        .unwrap_or_else(|| "not set".to_string())
                )));
            }
            lines.push(if busy {
                Line::styled("updating…", Style::default().fg(Color::Yellow))
            } else if s.update_available {
                Line::styled("UPDATE AVAILABLE", Style::default().fg(Color::Yellow))
            } else {
                Line::styled("up to date", Style::default().fg(Color::Green))
            });
            lines
        }
        Some(Err(e)) => vec![Line::styled(format!("error: {e}"), Style::default().fg(Color::Red))],
        None => vec![Line::raw("loading...")],
    }
}

/// A text progress bar line with a label.
fn bar_line(label: &str, done: u64, total: u64, bar_w: usize) -> String {
    if total == 0 {
        return format!("{label}: ?");
    }
    let ratio = (done as f64 / total as f64).clamp(0.0, 1.0);
    let filled = (bar_w as f64 * ratio).round() as usize;
    format!(
        "[{}{}] {:5.1}%  {}/{}  {label}",
        "█".repeat(filled),
        "░".repeat(bar_w - filled),
        ratio * 100.0,
        fmt_bytes(done),
        fmt_bytes(total),
    )
}

/// Human size with adaptive units (KiB / MiB / GiB).
fn fmt_bytes(b: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = b as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.0} KiB", b / KIB)
    }
}

/// Keep the tail of long file paths (the part that matters).
fn shorten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("…{}", chars[chars.len() - max + 1..].iter().collect::<String>())
    }
}

/// A centered rectangle for overlays.
fn centered_rect(percent_x: u16, percent_y: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use super::{
        body_choice, default_folder, parse_quality_arg, split_play_args, ui, InstallDraft, QualityDraft, Screen,
        UiState,
    };
    use kuro_core::{quality, BodyChoice, Game, GameStatus, Quality, Server};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Render the whole UI into a headless buffer and return it as plain text,
    /// so a panel's layout can be eyeballed/asserted without a real terminal.
    pub(super) fn render_to_string(state: &UiState, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| ui(f, state)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Synthetic game folder with one quality set present (HD) and the rest
    /// absent, mirroring a real WuWa install where only HD is downloaded.
    fn fake_wuwa(tag: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("kuro-tui-{tag}-"))
            .tempdir()
            .unwrap();
        let hd = dir.path().join("Client/Content/HD");
        std::fs::create_dir_all(&hd).unwrap();
        std::fs::write(hd.join("pakchunk0.pak"), vec![0u8; 4096]).unwrap();
        std::fs::write(hd.join("pakchunk1.pak"), vec![0u8; 8192]).unwrap();
        dir
    }

    #[test]
    fn dump_panels_for_inspection() {
        let dir = fake_wuwa("dump");
        let info = quality::info(dir.path());
        let mut state = UiState {
            paths: vec![dir.path().display().to_string()],
            logs: vec!["status: Wuthering Waves (cn) — up to date".into()],
            statuses: vec![None],
            quality_modal: Some(QualityDraft {
                game: Game::WuWa,
                choice: Quality::Hd,
                info,
            }),
            ..UiState::default()
        };
        println!("=== QUALITY PANEL ===\n{}", render_to_string(&state, 100, 34));

        state.quality_modal = None;
        state.show_help = true;
        println!("=== HELP PANEL ===\n{}", render_to_string(&state, 100, 40));

        state.show_help = false;
        state.install = Some(InstallDraft::new("/tmp/kuro-demo".into()));
        println!("=== INSTALL PANEL ===\n{}", render_to_string(&state, 100, 34));

        state.install = None;
        state.screen = Screen::Detail;
        println!("=== DETAIL ===\n{}", render_to_string(&state, 100, 34));
    }

    /// The help overlay is the densest panel; assert it fits (nothing clipped)
    /// at a typical 100x40 terminal, and that the quality panel spells out the
    /// plain-language state rather than leaking the `-krqlv` flag.
    #[test]
    fn panels_fit_and_read_plainly() {
        let mut state = UiState {
            show_help: true,
            ..UiState::default()
        };
        // 100x34 is the tight case the help must still fit inside.
        let help = render_to_string(&state, 100, 34);
        assert!(help.contains("Esc to close"), "help footer clipped:\n{help}");
        assert!(!help.contains("-krqlv"), "help leaks the raw flag:\n{help}");

        let dir = fake_wuwa("clarity");
        let info = quality::info(dir.path());
        state.show_help = false;
        state.quality_modal = Some(QualityDraft {
            game: Game::WuWa,
            choice: Quality::Hd,
            info,
        });
        let q = render_to_string(&state, 100, 34);
        assert!(q.contains("not installed"), "quality panel missing plain status:\n{q}");
        assert!(
            q.contains("Enter pick + install"),
            "quality panel missing the install hint:\n{q}"
        );
        assert!(!q.contains("-krqlv"), "quality panel leaks the raw flag:\n{q}");
    }

    /// `kuro play` with no folder used to panic on `&args[2..]`; the folder is
    /// optional and a leading flag is not a folder.
    #[test]
    fn play_args_split_folder_and_flags() {
        // Bare `kuro play`: default folder, nothing else — the panic case.
        let (folder, rest) = split_play_args(&args(&["play"]));
        assert_eq!(folder, default_folder());
        assert!(rest.is_empty(), "unexpected trailing args: {rest:?}");

        // Folder first, then the tier.
        let (folder, rest) = split_play_args(&args(&["play", "/games/wuwa", "hd"]));
        assert_eq!(folder, "/games/wuwa");
        assert_eq!(rest, vec!["hd"]);

        // A leading flag is not a folder.
        let (folder, rest) = split_play_args(&args(&["play", "--quality", "hd"]));
        assert_eq!(folder, default_folder());
        assert_eq!(rest, vec!["--quality", "hd"]);
    }

    #[test]
    fn quality_arg_accepts_flag_and_positional_forms() {
        assert_eq!(parse_quality_arg(&args(&[])), Ok(None));
        assert_eq!(parse_quality_arg(&args(&["hd"])), Ok(Some("hd")));
        assert_eq!(parse_quality_arg(&args(&["--quality", "uhd"])), Ok(Some("uhd")));
        assert_eq!(parse_quality_arg(&args(&["--quality=sd"])), Ok(Some("sd")));
        // flag form the old code silently mis-read as the literal tier string
        assert_eq!(parse_quality_arg(&args(&["--quality", "hd"])), Ok(Some("hd")));
    }

    #[test]
    fn quality_arg_rejects_dangling_flag() {
        assert_eq!(
            parse_quality_arg(&args(&["--quality"])),
            Err("--quality needs a value (sd|hd|uhd)")
        );
    }

    /// PGR takes no `-krqlv=` tier, so its box shows no quality line; WuWa's
    /// still does. (PGR used to render a meaningless "quality: not set".)
    #[test]
    fn status_quality_line_is_wuwa_only() {
        let state = |game, server| UiState {
            screen: Screen::Detail,
            paths: vec!["/games/x".into()],
            statuses: vec![Some(Ok(GameStatus {
                game,
                server,
                local_version: Some("1.0.0".into()),
                remote_version: "1.0.0".into(),
                update_available: false,
            }))],
            ..UiState::default()
        };

        let pgr = render_to_string(&state(Game::Pgr, Server::Global), 80, 30);
        assert!(!pgr.contains("quality:"), "PGR box shows a quality line:\n{pgr}");

        let wuwa = render_to_string(&state(Game::WuWa, Server::Cn), 80, 30);
        assert!(
            wuwa.contains("quality: not set"),
            "WuWa box lost its quality line:\n{wuwa}"
        );
    }

    fn two_games() -> UiState {
        let status = |game, server, local: &str, remote: &str| {
            Some(Ok(GameStatus {
                game,
                server,
                local_version: Some(local.into()),
                remote_version: remote.into(),
                update_available: local != remote,
            }))
        };
        UiState {
            paths: vec!["/games/Wuthering Waves".into(), "/games/Punishing Gray Raven".into()],
            statuses: vec![
                status(Game::WuWa, Server::Cn, "3.7.1", "3.8.0"),
                status(Game::Pgr, Server::Global, "4.9.0", "4.9.0"),
            ],
            quality_sel: vec![Some(Quality::Hd), None],
            ..UiState::default()
        }
    }

    /// The root screen lists both games as rows, never as side-by-side status
    /// boxes. This is the "one page, one game" contract at the list level.
    #[test]
    fn games_screen_lists_rows_without_detail_boxes() {
        let out = render_to_string(&two_games(), 100, 30);
        assert!(out.contains("GAME"), "games table header missing:\n{out}");
        assert!(out.contains("Wuthering Waves"), "WuWa row missing:\n{out}");
        assert!(out.contains("Punishing: Gray Raven"), "PGR row missing:\n{out}");
        assert!(
            out.contains("update available"),
            "the list lost the update state:\n{out}"
        );
        assert!(!out.contains("game:"), "the list leaked a detail status box:\n{out}");
        assert!(!out.contains("server:"), "the list leaked a detail status box:\n{out}");
    }

    /// The detail screen renders exactly one game. The other game must not be
    /// anywhere in the frame, which is the user's explicit requirement.
    #[test]
    fn detail_screen_shows_only_the_open_game() {
        let mut state = two_games();
        state.screen = Screen::Detail;
        state.active = 0;
        let out = render_to_string(&state, 100, 30);
        assert!(out.contains("Wuthering Waves"), "open game missing:\n{out}");
        assert!(
            !out.contains("Punishing: Gray Raven"),
            "another game is on the detail page:\n{out}"
        );
        assert!(out.contains("Esc back"), "detail page lost the back hint:\n{out}");
    }

    #[test]
    fn body_choice_maps_the_flag_to_the_pack_an_install_fetches() {
        // No flag: everything the channel serves (the historical behaviour).
        assert_eq!(body_choice(Some(Game::WuWa), &args(&[])), Ok(BodyChoice::All));
        assert_eq!(
            body_choice(Some(Game::WuWa), &args(&["--quality", "hd"])),
            Ok(BodyChoice::Only(Quality::Hd))
        );
        let err = body_choice(Some(Game::WuWa), &args(&["--quality", "medium"])).unwrap_err();
        assert!(err.contains("unknown quality `medium`"), "{err}");
    }

    #[test]
    fn body_choice_refuses_quality_packs_for_pgr() {
        // PGR's manifest has no Client/Content/<TIER> bodies — asking for one
        // must not quietly install the whole client instead.
        let err = body_choice(Some(Game::Pgr), &args(&["--quality", "hd"])).unwrap_err();
        assert!(err.contains("no quality packs"), "{err}");
        assert!(err.contains("Punishing: Gray Raven"), "{err}");
        // Without the flag PGR installs normally.
        assert_eq!(body_choice(Some(Game::Pgr), &args(&[])), Ok(BodyChoice::All));
    }

    #[test]
    fn install_dialog_cycles_packs_and_pgr_has_none() {
        let mut draft = InstallDraft::new("/tmp/kuro-demo".into());
        assert_eq!(draft.body, BodyChoice::All);
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::Only(Quality::Sd));
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::Only(Quality::Hd));
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::Only(Quality::Uhd));
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::All);

        // Switching the dialog to PGR resets the pack: PGR can serve none.
        draft.cycle_body();
        draft.game = Game::Pgr;
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::All);
    }
    /// The allocator must keep jemalloc's default 10 s dirty decay. A
    /// `_rjem_malloc_conf` override (the zero-decay experiment) would change
    /// this and raise the CPU cost; catch that here, not on a laptop.
    #[test]
    fn allocator_keeps_jemalloc_default_decay() {
        let dirty: isize = unsafe { tikv_jemalloc_ctl::raw::read(b"opt.dirty_decay_ms\0").unwrap() };
        assert_eq!(dirty, 10_000);
    }
}
